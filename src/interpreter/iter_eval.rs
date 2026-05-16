//! Iterator-source pull-loop, adapter-chain stepping, draining, and peeking.
//!
//! Houses `iterator_step` (lazy adapter dispatch — Map/Filter/Take/Skip/…),
//! `pull_source` (raw source consumption for Vec/Map/Set/Range/etc.),
//! `drain_source` (eager-consume by-ref iterators), and `peek_value`
//! (probe the next item without consuming).
//!
//! Lives in a sibling `impl<'a> super::Interpreter<'a>` block.

use super::value::{EnumData, IteratorSource, IteratorStep, Value};

impl<'a> super::Interpreter<'a> {
    /// Pull the next element from a `Value::Iterator`, applying its lazy
    /// adaptor chain (`Map` / `Filter` / future). Returns `None` when
    /// exhausted; callers are responsible for any state-write-back of
    /// the modified iterator value to their bindings.
    ///
    /// `Filter` may reject items, so the body loops until either an item
    /// passes every step or the source runs out. The adaptor closures
    /// can mutate captured outer bindings (via `mut ref` capture); the
    /// iterator's own state (items / cursor / steps) is parameter data,
    /// not on `self`, so the borrow checker tolerates the nested call.
    pub(crate) fn iterator_step(&mut self, iter: &mut Value) -> Option<Value> {
        // Snapshot the step chain once so the per-element loop doesn't
        // hold a borrow on `*iter` across `invoke_function_value` calls.
        // Stateful steps (Enumerate / Take / Skip) mutate this clone in
        // place; whatever state changes survive — closure rejection,
        // `take` exhaustion, multiple pulls in one call — get written
        // back to the iterator's stored chain just before return.
        let mut steps = match iter {
            Value::Iterator { steps, .. } => steps.clone(),
            _ => return None,
        };
        let yielded = 'pull: loop {
            let Some(raw_item) = self.pull_source(iter) else {
                break 'pull None;
            };
            let mut item = raw_item;
            let mut keep = true;
            let mut stop = false;
            for step in steps.iter_mut() {
                match step {
                    IteratorStep::Map(f) => {
                        item = self.invoke_function_value(f.clone(), vec![item]);
                    }
                    IteratorStep::Filter(pred) => {
                        let result = self.invoke_function_value(pred.clone(), vec![item.clone()]);
                        if !matches!(result, Value::Bool(true)) {
                            keep = false;
                            break;
                        }
                    }
                    IteratorStep::Enumerate(idx) => {
                        item = Value::Tuple(vec![Value::Int(*idx as i64), item]);
                        *idx += 1;
                    }
                    IteratorStep::Take(remaining) => {
                        if *remaining == 0 {
                            stop = true;
                            keep = false;
                            break;
                        }
                        *remaining -= 1;
                    }
                    IteratorStep::Skip(remaining) => {
                        if *remaining > 0 {
                            *remaining -= 1;
                            keep = false;
                            break;
                        }
                    }
                    IteratorStep::TakeWhile { pred, done } => {
                        if *done {
                            // Sticky-stop: predicate already tripped on
                            // an earlier element, so every subsequent
                            // pull short-circuits without firing pred.
                            stop = true;
                            keep = false;
                            break;
                        }
                        let result = self.invoke_function_value(pred.clone(), vec![item.clone()]);
                        if !matches!(result, Value::Bool(true)) {
                            *done = true;
                            stop = true;
                            keep = false;
                            break;
                        }
                    }
                    IteratorStep::SkipWhile { pred, done } => {
                        if *done {
                            // Sticky-pass: predicate failed on an
                            // earlier element, so every subsequent
                            // item goes through unconditionally.
                            continue;
                        }
                        let result = self.invoke_function_value(pred.clone(), vec![item.clone()]);
                        if matches!(result, Value::Bool(true)) {
                            keep = false;
                            break;
                        }
                        *done = true;
                    }
                    IteratorStep::StepBy { n, remaining_skip } => {
                        if *remaining_skip > 0 {
                            *remaining_skip -= 1;
                            keep = false;
                            break;
                        }
                        // Yield this item, then skip the next n-1.
                        // n ≥ 1 by construction (clamped at dispatch),
                        // so the subtraction never underflows.
                        *remaining_skip = *n - 1;
                    }
                    IteratorStep::Inspect(f) => {
                        // Side-effect-only step: invoke f and discard
                        // the result; the item passes through.
                        self.invoke_function_value(f.clone(), vec![item.clone()]);
                    }
                    IteratorStep::Scan { f, state, done } => {
                        if *done {
                            stop = true;
                            keep = false;
                            break;
                        }
                        let result = self
                            .invoke_function_value(f.clone(), vec![state.clone(), item.clone()]);
                        // Closure returns Option<(A, U)>: Some carries
                        // (new_state, yielded); None signals stop.
                        let parsed = match result {
                            Value::EnumVariant {
                                variant,
                                data: EnumData::Tuple(mut vals),
                                ..
                            } if variant == "Some" && vals.len() == 1 => match vals.remove(0) {
                                Value::Tuple(mut tuple) if tuple.len() == 2 => {
                                    let yielded = tuple.remove(1);
                                    let new_state = tuple.remove(0);
                                    Some((new_state, yielded))
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        match parsed {
                            Some((new_state, yielded)) => {
                                *state = new_state;
                                item = yielded;
                            }
                            None => {
                                *done = true;
                                stop = true;
                                keep = false;
                                break;
                            }
                        }
                    }
                }
            }
            if stop {
                // `take` exhaustion — drain the source so subsequent
                // calls also return None without touching downstream
                // adaptor state.
                self.drain_source(iter);
                break 'pull None;
            }
            if keep {
                break 'pull Some(item);
            }
        };
        // Write the (possibly mutated) step chain back so per-call
        // counter state persists across `next()` pulls.
        if let Value::Iterator {
            steps: stored_steps,
            ..
        } = iter
        {
            *stored_steps = steps;
        }
        yielded
    }

    /// Pull the next raw item from an iterator's source layer. Eager
    /// walks `items[cursor]`; Chain advances through its parts, calling
    /// `iterator_step` recursively on each so per-part adaptor chains
    /// fire; Zip pulls from both sides in lockstep, yielding a tuple or
    /// stopping when either side ends.
    pub(crate) fn pull_source(&mut self, iter: &mut Value) -> Option<Value> {
        let Value::Iterator { source, .. } = iter else {
            return None;
        };
        match source {
            IteratorSource::Eager { items, cursor } => {
                if *cursor >= items.len() {
                    return None;
                }
                let it = items[*cursor].clone();
                *cursor += 1;
                Some(it)
            }
            IteratorSource::Chain { .. } => {
                // Walk the current part until it yields or exhausts; on
                // exhaust, advance to the next. Take parts out of the
                // source while recursing so we can pass `&mut self` to
                // iterator_step without aliasing the iter binding.
                loop {
                    let Value::Iterator {
                        source: IteratorSource::Chain { parts, current },
                        ..
                    } = iter
                    else {
                        return None;
                    };
                    if *current >= parts.len() {
                        return None;
                    }
                    let idx = *current;
                    let mut part = std::mem::replace(&mut parts[idx], Value::Unit);
                    let yielded = self.iterator_step(&mut part);
                    let Value::Iterator {
                        source: IteratorSource::Chain { parts, current },
                        ..
                    } = iter
                    else {
                        return None;
                    };
                    parts[idx] = part;
                    if yielded.is_some() {
                        return yielded;
                    }
                    *current += 1;
                }
            }
            IteratorSource::Zip { .. } => {
                // Take both sides out so we can pass &mut self into
                // iterator_step twice without aliasing the iter binding.
                let (mut left, mut right) = if let Value::Iterator {
                    source: IteratorSource::Zip { left, right },
                    ..
                } = iter
                {
                    (
                        std::mem::replace(left.as_mut(), Value::Unit),
                        std::mem::replace(right.as_mut(), Value::Unit),
                    )
                } else {
                    return None;
                };
                let l = self.iterator_step(&mut left);
                let r = self.iterator_step(&mut right);
                if let Value::Iterator {
                    source:
                        IteratorSource::Zip {
                            left: l_box,
                            right: r_box,
                        },
                    ..
                } = iter
                {
                    **l_box = left;
                    **r_box = right;
                }
                match (l, r) {
                    (Some(a), Some(b)) => Some(Value::Tuple(vec![a, b])),
                    _ => None,
                }
            }
            IteratorSource::FlatMap { .. } => {
                // Drain the in-flight inner iterator first; if it
                // yields, that's our item. If exhausted, advance the
                // outer (recursively iterator_step on it), apply f to
                // the outer item, store the resulting iterator as the
                // new inner, and retry. Same `mem::replace` ceremony
                // as Zip: pull each sub-iterator out of the source,
                // recurse with `&mut self`, write back.
                loop {
                    let inner_yield = if let Value::Iterator {
                        source: IteratorSource::FlatMap { current_inner, .. },
                        ..
                    } = iter
                    {
                        if let Some(boxed) = current_inner.as_mut() {
                            let mut inner = std::mem::replace(boxed.as_mut(), Value::Unit);
                            let yielded = self.iterator_step(&mut inner);
                            if let Value::Iterator {
                                source: IteratorSource::FlatMap { current_inner, .. },
                                ..
                            } = iter
                            {
                                if let Some(boxed) = current_inner.as_mut() {
                                    **boxed = inner;
                                }
                            }
                            Some(yielded)
                        } else {
                            None
                        }
                    } else {
                        return None;
                    };
                    if let Some(Some(v)) = inner_yield {
                        return Some(v);
                    }
                    if let Value::Iterator {
                        source: IteratorSource::FlatMap { current_inner, .. },
                        ..
                    } = iter
                    {
                        *current_inner = None;
                    }
                    let outer_yield = if let Value::Iterator {
                        source: IteratorSource::FlatMap { outer, .. },
                        ..
                    } = iter
                    {
                        let mut o = std::mem::replace(outer.as_mut(), Value::Unit);
                        let yielded = self.iterator_step(&mut o);
                        if let Value::Iterator {
                            source: IteratorSource::FlatMap { outer, .. },
                            ..
                        } = iter
                        {
                            **outer = o;
                        }
                        yielded
                    } else {
                        return None;
                    };
                    let item = outer_yield?;
                    let f_clone = if let Value::Iterator {
                        source: IteratorSource::FlatMap { f, .. },
                        ..
                    } = iter
                    {
                        (**f).clone()
                    } else {
                        return None;
                    };
                    let new_inner = self.invoke_function_value(f_clone, vec![item]);
                    if !matches!(new_inner, Value::Iterator { .. }) {
                        return None;
                    }
                    if let Value::Iterator {
                        source: IteratorSource::FlatMap { current_inner, .. },
                        ..
                    } = iter
                    {
                        *current_inner = Some(Box::new(new_inner));
                    }
                }
            }
            IteratorSource::Cycle { .. } => {
                // Pull from `current`. If yielded, return. If
                // exhausted, replace `current` with a fresh
                // `template.clone()` and try once more — if THAT
                // also yields None, the template is empty; set
                // `exhausted = true` and stop forever (avoids the
                // infinite-empty-loop trap).
                if let Value::Iterator {
                    source: IteratorSource::Cycle { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let first = if let Value::Iterator {
                    source: IteratorSource::Cycle { current, .. },
                    ..
                } = iter
                {
                    let mut c = std::mem::replace(current.as_mut(), Value::Unit);
                    let y = self.iterator_step(&mut c);
                    if let Value::Iterator {
                        source: IteratorSource::Cycle { current, .. },
                        ..
                    } = iter
                    {
                        **current = c;
                    }
                    y
                } else {
                    return None;
                };
                if first.is_some() {
                    return first;
                }
                // Reset to a fresh template clone.
                let fresh = if let Value::Iterator {
                    source: IteratorSource::Cycle { template, .. },
                    ..
                } = iter
                {
                    (**template).clone()
                } else {
                    return None;
                };
                if let Value::Iterator {
                    source: IteratorSource::Cycle { current, .. },
                    ..
                } = iter
                {
                    **current = fresh;
                }
                let second = if let Value::Iterator {
                    source: IteratorSource::Cycle { current, .. },
                    ..
                } = iter
                {
                    let mut c = std::mem::replace(current.as_mut(), Value::Unit);
                    let y = self.iterator_step(&mut c);
                    if let Value::Iterator {
                        source: IteratorSource::Cycle { current, .. },
                        ..
                    } = iter
                    {
                        **current = c;
                    }
                    y
                } else {
                    return None;
                };
                if second.is_some() {
                    return second;
                }
                // Template is empty — sticky-stop.
                if let Value::Iterator {
                    source: IteratorSource::Cycle { exhausted, .. },
                    ..
                } = iter
                {
                    *exhausted = true;
                }
                None
            }
            IteratorSource::Peekable { .. } => {
                // Drain the buffered slot first; on miss, recurse into
                // `inner` via iterator_step. `mem::replace` ceremony
                // mirrors Chain/Zip so we can pass `&mut self` into
                // iterator_step without aliasing the iter binding.
                if let Value::Iterator {
                    source: IteratorSource::Peekable { buffered, .. },
                    ..
                } = iter
                {
                    if let Some(boxed) = buffered.take() {
                        return Some(*boxed);
                    }
                }
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return None;
                };
                let yielded = self.iterator_step(&mut inner_taken);
                if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, .. },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                }
                yielded
            }
            IteratorSource::Chunks { .. } => {
                // Pull up to n items from inner; emit a fresh Vec.
                // Sticky-stop once we get an empty chunk (inner
                // exhausted with nothing in flight). Heap allocation
                // is the per-chunk Vec; effect-checker carries
                // `allocates(Heap)`.
                if let Value::Iterator {
                    source: IteratorSource::Chunks { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let n = if let Value::Iterator {
                    source: IteratorSource::Chunks { n, .. },
                    ..
                } = iter
                {
                    *n
                } else {
                    return None;
                };
                let mut chunk: Vec<Value> = Vec::with_capacity(n);
                for _ in 0..n {
                    let mut inner_taken = if let Value::Iterator {
                        source: IteratorSource::Chunks { inner, .. },
                        ..
                    } = iter
                    {
                        std::mem::replace(inner.as_mut(), Value::Unit)
                    } else {
                        return None;
                    };
                    let pulled = self.iterator_step(&mut inner_taken);
                    if let Value::Iterator {
                        source: IteratorSource::Chunks { inner, .. },
                        ..
                    } = iter
                    {
                        **inner = inner_taken;
                    }
                    match pulled {
                        Some(v) => chunk.push(v),
                        None => break,
                    }
                }
                if chunk.is_empty() {
                    if let Value::Iterator {
                        source: IteratorSource::Chunks { exhausted, .. },
                        ..
                    } = iter
                    {
                        *exhausted = true;
                    }
                    None
                } else {
                    if chunk.len() < n {
                        if let Value::Iterator {
                            source: IteratorSource::Chunks { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                    }
                    Some(Value::array_of(chunk))
                }
            }
            IteratorSource::Windows { .. } => {
                // Sliding window of size n. First pull primes the
                // buffer by collecting n items; subsequent pulls
                // drop the front and push one new item. If the
                // source has fewer than n items at any priming /
                // refill point, sticky-stop (no partial windows).
                if let Value::Iterator {
                    source: IteratorSource::Windows { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let (n, primed) = if let Value::Iterator {
                    source: IteratorSource::Windows { n, primed, .. },
                    ..
                } = iter
                {
                    (*n, *primed)
                } else {
                    return None;
                };
                if !primed {
                    // Prime: pull n items.
                    let mut filled = 0usize;
                    for _ in 0..n {
                        let mut inner_taken = if let Value::Iterator {
                            source: IteratorSource::Windows { inner, .. },
                            ..
                        } = iter
                        {
                            std::mem::replace(inner.as_mut(), Value::Unit)
                        } else {
                            return None;
                        };
                        let pulled = self.iterator_step(&mut inner_taken);
                        if let Value::Iterator {
                            source: IteratorSource::Windows { inner, .. },
                            ..
                        } = iter
                        {
                            **inner = inner_taken;
                        }
                        match pulled {
                            Some(v) => {
                                if let Value::Iterator {
                                    source: IteratorSource::Windows { buffer, .. },
                                    ..
                                } = iter
                                {
                                    buffer.push(v);
                                    filled += 1;
                                }
                            }
                            None => break,
                        }
                    }
                    if filled < n {
                        if let Value::Iterator {
                            source: IteratorSource::Windows { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                        return None;
                    }
                    if let Value::Iterator {
                        source: IteratorSource::Windows { primed, buffer, .. },
                        ..
                    } = iter
                    {
                        *primed = true;
                        return Some(Value::array_of(buffer.clone()));
                    }
                    return None;
                }
                // Already primed — pull one item and slide.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Windows { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return None;
                };
                let pulled = self.iterator_step(&mut inner_taken);
                if let Value::Iterator {
                    source: IteratorSource::Windows { inner, .. },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                }
                match pulled {
                    Some(v) => {
                        if let Value::Iterator {
                            source: IteratorSource::Windows { buffer, .. },
                            ..
                        } = iter
                        {
                            buffer.remove(0);
                            buffer.push(v);
                            return Some(Value::array_of(buffer.clone()));
                        }
                        None
                    }
                    None => {
                        if let Value::Iterator {
                            source: IteratorSource::Windows { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                        None
                    }
                }
            }
            IteratorSource::ChunkBy { .. } => {
                // Build one group per pull: seed from any pending
                // (item, key) carried over from the previous pull
                // (the lookahead element that triggered the last
                // group boundary), then keep pulling from `inner`
                // and applying `key_fn` until the key changes (stash
                // that item as the next pending) or the inner
                // exhausts (set sticky-exhausted and emit the
                // trailing group). Heap allocation is the per-group
                // `Vec`; effect-checker carries `allocates(Heap)`.
                if let Value::Iterator {
                    source: IteratorSource::ChunkBy { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let mut group: Vec<Value> = Vec::new();
                let mut group_key: Option<Value> = None;
                if let Value::Iterator {
                    source:
                        IteratorSource::ChunkBy {
                            pending_item,
                            pending_key,
                            ..
                        },
                    ..
                } = iter
                {
                    if let (Some(item_box), Some(key_box)) =
                        (pending_item.take(), pending_key.take())
                    {
                        group.push(*item_box);
                        group_key = Some(*key_box);
                    }
                }
                loop {
                    let mut inner_taken = if let Value::Iterator {
                        source: IteratorSource::ChunkBy { inner, .. },
                        ..
                    } = iter
                    {
                        std::mem::replace(inner.as_mut(), Value::Unit)
                    } else {
                        return None;
                    };
                    let pulled = self.iterator_step(&mut inner_taken);
                    if let Value::Iterator {
                        source: IteratorSource::ChunkBy { inner, .. },
                        ..
                    } = iter
                    {
                        **inner = inner_taken;
                    }
                    let Some(item) = pulled else {
                        // Inner exhausted — sticky-stop and emit the
                        // final group if non-empty.
                        if let Value::Iterator {
                            source: IteratorSource::ChunkBy { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                        if group.is_empty() {
                            return None;
                        } else {
                            return Some(Value::array_of(group));
                        }
                    };
                    let key_fn = if let Value::Iterator {
                        source: IteratorSource::ChunkBy { key_fn, .. },
                        ..
                    } = iter
                    {
                        (**key_fn).clone()
                    } else {
                        return None;
                    };
                    let key = self.invoke_function_value(key_fn, vec![item.clone()]);
                    match &group_key {
                        None => {
                            // First element of a fresh group.
                            group.push(item);
                            group_key = Some(key);
                        }
                        Some(prev) if *prev == key => {
                            group.push(item);
                        }
                        Some(_) => {
                            // Key change — stash this item (with its
                            // already-computed key) as the seed for
                            // the next pull, return current group.
                            if let Value::Iterator {
                                source:
                                    IteratorSource::ChunkBy {
                                        pending_item,
                                        pending_key,
                                        ..
                                    },
                                ..
                            } = iter
                            {
                                *pending_item = Some(Box::new(item));
                                *pending_key = Some(Box::new(key));
                            }
                            return Some(Value::array_of(group));
                        }
                    }
                }
            }
        }
    }

    /// Force an iterator's source to "exhausted" — used by `take(0)` so
    /// subsequent pulls return None without re-firing downstream adaptors.
    pub(crate) fn drain_source(&mut self, iter: &mut Value) {
        let Value::Iterator { source, .. } = iter else {
            return;
        };
        match source {
            IteratorSource::Eager { items, cursor } => *cursor = items.len(),
            IteratorSource::Chain { parts, current } => *current = parts.len(),
            IteratorSource::Zip { left, right } => {
                let mut l = std::mem::replace(left.as_mut(), Value::Unit);
                let mut r = std::mem::replace(right.as_mut(), Value::Unit);
                self.drain_source(&mut l);
                self.drain_source(&mut r);
                if let Value::Iterator {
                    source:
                        IteratorSource::Zip {
                            left: l_box,
                            right: r_box,
                        },
                    ..
                } = iter
                {
                    **l_box = l;
                    **r_box = r;
                }
            }
            IteratorSource::FlatMap { outer, .. } => {
                // Drain the outer and clear the in-flight inner;
                // pull_source's loop returns None at the outer-pull
                // step on every subsequent call.
                let mut o = std::mem::replace(outer.as_mut(), Value::Unit);
                self.drain_source(&mut o);
                if let Value::Iterator {
                    source:
                        IteratorSource::FlatMap {
                            outer,
                            current_inner,
                            ..
                        },
                    ..
                } = iter
                {
                    **outer = o;
                    *current_inner = None;
                }
            }
            IteratorSource::Cycle { exhausted, .. } => {
                // Just trip the sticky-stop flag; pull_source's
                // first check returns None on every subsequent call.
                *exhausted = true;
            }
            IteratorSource::Peekable { .. } => {
                // Drain the inner and clear any buffered element. After
                // this, pull_source: buffered is None → falls through
                // to the inner pull which returns None forever.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, buffered },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    *buffered = None;
                }
            }
            IteratorSource::Chunks { .. } => {
                // Drain the inner and trip sticky-exhausted.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Chunks { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source:
                        IteratorSource::Chunks {
                            inner, exhausted, ..
                        },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    *exhausted = true;
                }
            }
            IteratorSource::Windows { .. } => {
                // Drain the inner, clear the rolling buffer, trip
                // sticky-exhausted.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Windows { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source:
                        IteratorSource::Windows {
                            inner,
                            buffer,
                            exhausted,
                            ..
                        },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    buffer.clear();
                    *exhausted = true;
                }
            }
            IteratorSource::ChunkBy { .. } => {
                // Drain the inner and trip the sticky-exhausted flag;
                // also clear any in-flight pending so the trailing
                // group isn't emitted after a forced drain.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::ChunkBy { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source:
                        IteratorSource::ChunkBy {
                            inner,
                            pending_item,
                            pending_key,
                            exhausted,
                            ..
                        },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    *pending_item = None;
                    *pending_key = None;
                    *exhausted = true;
                }
            }
        }
    }

    /// `Peekable.peek()` — look one element ahead without consuming.
    /// Returns `Option<T>` (Some/None Value::EnumVariant). Pulls from
    /// the buffered slot if present; otherwise pulls one element from
    /// the inner iterator via `iterator_step`, stores it in the
    /// buffer, and returns a clone. The buffer stays populated so the
    /// next `peek()` (or `next()`) sees the same element. Once the
    /// inner is exhausted and the buffer is empty, returns
    /// `None` on every subsequent call.
    pub(crate) fn peek_value(&mut self, iter: &mut Value) -> Value {
        let some = |v: Value| Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "Some".to_string(),
            data: EnumData::Tuple(vec![v]),
        };
        let none = || Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "None".to_string(),
            data: EnumData::Unit,
        };
        if let Value::Iterator {
            source: IteratorSource::Peekable { buffered, .. },
            ..
        } = iter
        {
            if let Some(boxed) = buffered.as_ref() {
                return some((**boxed).clone());
            }
        }
        let mut inner_taken = if let Value::Iterator {
            source: IteratorSource::Peekable { inner, .. },
            ..
        } = iter
        {
            std::mem::replace(inner.as_mut(), Value::Unit)
        } else {
            return none();
        };
        let yielded = self.iterator_step(&mut inner_taken);
        if let Value::Iterator {
            source: IteratorSource::Peekable { inner, buffered },
            ..
        } = iter
        {
            **inner = inner_taken;
            match yielded {
                Some(v) => {
                    *buffered = Some(Box::new(v.clone()));
                    some(v)
                }
                None => none(),
            }
        } else {
            none()
        }
    }
}
