//! Argument-label validation shared by stdlib method-inference paths.
//!
//! The per-stdlib-type method-resolution arms invoked from
//! `infer_method_call` live in sibling submodules:
//! `stdlib_seq` (String, Slice), `stdlib_map` (Map / Entry / SortedSet /
//! Set), `stdlib_iter` (Iterator), and `stdlib_io` (Regex / HTTP /
//! channels). All of them call into `validate_labels` defined here.

use crate::ast::*;
use crate::token::Span;

use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    // ── Label Validation ────────────────────────────────────────

    pub(super) fn validate_labels(
        &mut self,
        args: &[CallArg],
        param_names: &[Option<String>],
        _span: &Span,
    ) {
        let mut seen_label = false;
        let mut seen_unlabeled_after_label = false;

        for (i, arg) in args.iter().enumerate() {
            if let Some(ref label) = arg.label {
                if seen_unlabeled_after_label {
                    self.type_error(
                        "labeled arguments must be contiguous — cannot have unlabeled arguments between labeled ones".to_string(),
                        arg.span.clone(),
                        TypeErrorKind::NonContiguousLabels,
                    );
                }
                seen_label = true;

                // Check label matches parameter name at this position
                if i < param_names.len() {
                    if let Some(ref pname) = param_names[i] {
                        if label != pname {
                            self.type_error(
                                format!(
                                    "label '{}' does not match parameter '{}' at position {}",
                                    label,
                                    pname,
                                    i + 1
                                ),
                                arg.span.clone(),
                                TypeErrorKind::LabelMismatch,
                            );
                        }
                    } else {
                        self.type_error(
                            format!("parameter at position {} cannot be labeled (destructuring pattern)", i + 1),
                            arg.span.clone(),
                            TypeErrorKind::LabelMismatch,
                        );
                    }
                }
            } else if seen_label {
                seen_unlabeled_after_label = true;
            }
        }
    }
}
