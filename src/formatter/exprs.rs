//! Expression printing for the canonical formatter.

use crate::ast::*;
use std::fmt::Write;

use super::{escape_string, float_suffix_str, int_suffix_str};

impl super::Formatter {
    pub(super) fn format_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Integer(n, sfx) => {
                write!(self.output, "{n}").unwrap();
                if let Some(s) = sfx {
                    self.write_str(int_suffix_str(*s));
                }
            }
            ExprKind::Float(f, sfx) => {
                write!(self.output, "{f}").unwrap();
                if let Some(s) = sfx {
                    self.write_str(float_suffix_str(*s));
                }
            }
            ExprKind::CharLit(c) => write!(self.output, "'{c}'").unwrap(),
            ExprKind::StringLit(s) => {
                self.write_str("\"");
                self.write_str(&escape_string(s));
                self.write_str("\"");
            }
            ExprKind::MultiStringLit(s) => {
                // Multi-line strings keep their format
                self.write_str("\"\"\"");
                self.write_str(s);
                self.write_str("\"\"\"");
            }
            ExprKind::InterpolatedStringLit(parts) => {
                self.write_str("f\"");
                for part in parts {
                    match part {
                        crate::ast::ParsedInterpolationPart::Text(s) => {
                            self.write_str(&escape_string(s))
                        }
                        crate::ast::ParsedInterpolationPart::Expr(e) => {
                            self.write_str("{");
                            self.format_expr(e);
                            self.write_str("}");
                        }
                    }
                }
                self.write_str("\"");
            }
            ExprKind::Bool(b) => self.write_str(if *b { "true" } else { "false" }),
            ExprKind::Identifier(name) => self.write_ident(name),
            ExprKind::Path { segments, .. } => self.write_path(segments),
            ExprKind::SelfValue => self.write_str("self"),
            ExprKind::SelfType => self.write_str("Self"),

            ExprKind::Binary { op, left, right } => {
                self.format_expr(left);
                self.write_str(match op {
                    BinOp::Add => " + ",
                    BinOp::Sub => " - ",
                    BinOp::Mul => " * ",
                    BinOp::Div => " / ",
                    BinOp::Mod => " % ",
                    BinOp::Eq => " == ",
                    BinOp::NotEq => " != ",
                    BinOp::Lt => " < ",
                    BinOp::LtEq => " <= ",
                    BinOp::Gt => " > ",
                    BinOp::GtEq => " >= ",
                    BinOp::And => " and ",
                    BinOp::Or => " or ",
                    BinOp::BitAnd => " & ",
                    BinOp::BitOr => " | ",
                    BinOp::BitXor => " ^ ",
                    BinOp::Shl => " << ",
                    BinOp::Shr => " >> ",
                    BinOp::Range => "..",
                    BinOp::RangeInclusive => "..=",
                });
                self.format_expr(right);
            }
            ExprKind::Unary { op, operand } => {
                self.write_str(match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "not ",
                    UnaryOp::BitNot => "~",
                    UnaryOp::Deref => "*",
                });
                self.format_expr(operand);
            }
            ExprKind::Question(inner) => {
                self.format_expr(inner);
                self.write_str("?");
            }
            ExprKind::OptionalChain {
                object,
                field_or_method,
                args,
            } => {
                self.format_expr(object);
                self.write_str("?.");
                self.write_ident(field_or_method);
                if let Some(ref a) = args {
                    self.write_str("(");
                    self.format_call_args(a);
                    self.write_str(")");
                }
            }
            ExprKind::NilCoalesce { left, right } => {
                self.format_expr(left);
                self.write_str(" ?? ");
                self.format_expr(right);
            }
            ExprKind::Call { callee, args } => {
                self.format_expr(callee);
                self.write_str("(");
                self.format_call_args(args);
                self.write_str(")");
            }
            ExprKind::MethodCall {
                object,
                method,
                turbofish,
                args,
            } => {
                self.format_expr(object);
                self.write_str(".");
                self.write_ident(method);
                if let Some(ref tf) = turbofish {
                    self.write_str("[");
                    for (i, t) in tf.iter().enumerate() {
                        if i > 0 {
                            self.write_str(", ");
                        }
                        self.format_type_expr(t);
                    }
                    self.write_str("]");
                }
                self.write_str("(");
                self.format_call_args(args);
                self.write_str(")");
            }
            ExprKind::FieldAccess { object, field } => {
                self.format_expr(object);
                self.write_str(".");
                self.write_ident(field);
            }
            ExprKind::TupleIndex { object, index } => {
                self.format_expr(object);
                write!(self.output, ".{index}").unwrap();
            }
            ExprKind::Index { object, index } => {
                self.format_expr(object);
                self.write_str("[");
                self.format_expr(index);
                self.write_str("]");
            }
            ExprKind::Block(block) => self.format_block(block),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.write_str("if ");
                self.format_expr(condition);
                self.write_str(" ");
                self.format_block(then_block);
                if let Some(ref eb) = else_branch {
                    self.write_str(" else ");
                    match &eb.kind {
                        ExprKind::If { .. } | ExprKind::IfLet { .. } => self.format_expr(eb),
                        ExprKind::Block(block) => self.format_block(block),
                        _ => self.format_expr(eb),
                    }
                }
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.write_str("if let ");
                self.format_pattern(pattern);
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(" ");
                self.format_block(then_block);
                if let Some(ref eb) = else_branch {
                    self.write_str(" else ");
                    match &eb.kind {
                        ExprKind::Block(block) => self.format_block(block),
                        _ => self.format_expr(eb),
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.write_str("match ");
                self.format_expr(scrutinee);
                self.write_str(" {\n");
                self.push_indent();
                for arm in arms {
                    self.write_indent();
                    self.format_pattern(&arm.pattern);
                    if let Some(ref guard) = arm.guard {
                        self.write_str(" if ");
                        self.format_expr(guard);
                    }
                    self.write_str(" => ");
                    self.format_expr(&arm.body);
                    self.write_str(",\n");
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("}");
            }
            ExprKind::While {
                label,
                condition,
                body,
            } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("while ");
                self.format_expr(condition);
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::WhileLet {
                label,
                pattern,
                value,
                body,
            } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("while let ");
                self.format_pattern(pattern);
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::For {
                label,
                pattern,
                iterable,
                body,
            } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("for ");
                self.format_pattern(pattern);
                self.write_str(" in ");
                self.format_expr(iterable);
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::Loop { label, body } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("loop ");
                self.format_block(body);
            }
            ExprKind::LabeledBlock { label, body, .. } => {
                write!(self.output, "{label}: ").unwrap();
                self.format_block(body);
            }
            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            } => {
                match capture_mode {
                    Some(CaptureMode::Own) => self.write_str("own "),
                    Some(CaptureMode::Ref) => self.write_str("ref "),
                    Some(CaptureMode::MutRef) => self.write_str("mut ref "),
                    None => {}
                }
                self.write_str("|");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_pattern(&p.pattern);
                    if let Some(ref t) = p.ty {
                        self.write_str(": ");
                        self.format_type_expr(t);
                    }
                }
                self.write_str("| ");
                self.format_expr(body);
            }
            ExprKind::Return(val) => {
                self.write_str("return");
                if let Some(ref v) = val {
                    self.write_str(" ");
                    self.format_expr(v);
                }
            }
            ExprKind::Break { label, value } => {
                self.write_str("break");
                if let Some(ref l) = label {
                    write!(self.output, " '{l}").unwrap();
                }
                if let Some(ref v) = value {
                    self.write_str(" ");
                    self.format_expr(v);
                }
            }
            ExprKind::Continue { label } => {
                self.write_str("continue");
                if let Some(ref l) = label {
                    write!(self.output, " '{l}").unwrap();
                }
            }
            ExprKind::Tuple(elems) => {
                self.write_str("(");
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_expr(e);
                }
                if elems.len() == 1 {
                    self.write_str(",");
                }
                self.write_str(")");
            }
            ExprKind::ArrayLiteral(elems) => {
                self.write_str("[");
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_expr(e);
                }
                self.write_str("]");
            }
            ExprKind::PrefixCollectionLiteral { type_name, items } => {
                self.write_str(type_name);
                self.write_str("[");
                for (i, e) in items.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_expr(e);
                }
                self.write_str("]");
            }
            ExprKind::RepeatLiteral {
                type_name,
                value,
                count,
            } => {
                if let Some(name) = type_name {
                    self.write_ident(name);
                }
                self.write_str("[");
                self.format_expr(value);
                self.write_str("; ");
                self.format_expr(count);
                self.write_str("]");
            }
            ExprKind::MapLiteral(entries) => {
                if entries.is_empty() {
                    self.write_str("{:}");
                    return;
                }
                self.write_str("{\n");
                self.push_indent();
                for (k, v) in entries {
                    self.write_indent();
                    self.format_expr(k);
                    self.write_str(": ");
                    self.format_expr(v);
                    self.write_str(",\n");
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("}");
            }
            ExprKind::StructLiteral {
                path,
                fields,
                spread,
            } => {
                self.write_path(path);
                self.write_str(" {\n");
                self.push_indent();
                for fi in fields {
                    self.write_indent();
                    if fi.shorthand {
                        self.write_ident(&fi.name);
                    } else {
                        self.write_ident(&fi.name);
                        self.write_str(": ");
                        self.format_expr(&fi.value);
                    }
                    self.write_str(",\n");
                }
                if let Some(ref s) = spread {
                    self.write_indent();
                    self.write_str("..");
                    self.format_expr(s);
                    self.output.push('\n');
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("}");
            }
            ExprKind::Pipe { left, right } => {
                self.format_expr(left);
                self.write_str(" |> ");
                self.format_expr(right);
            }
            ExprKind::PipePlaceholder => self.write_str("_"),
            ExprKind::Cast { expr, ty } => {
                self.format_expr(expr);
                self.write_str(" as ");
                self.format_type_expr(ty);
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                if let Some(s) = start {
                    self.format_expr(s);
                }
                if *inclusive {
                    self.write_str("..=");
                } else {
                    self.write_str("..");
                }
                if let Some(e) = end {
                    self.format_expr(e);
                }
            }
            ExprKind::Unsafe(block) => {
                self.write_str("unsafe ");
                self.format_block(block);
            }
            ExprKind::Try(block) => {
                self.write_str("try ");
                self.format_block(block);
            }
            ExprKind::Seq(block) => {
                self.write_str("seq ");
                self.format_block(block);
            }
            ExprKind::Par(block) => {
                self.write_str("par ");
                self.format_block(block);
            }
            ExprKind::Lock { mutex, alias, body } => {
                self.write_str("lock ");
                self.write_str(mutex);
                if let Some(ref a) = alias {
                    self.write_str(" as ");
                    self.write_str(a);
                }
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::Providers { bindings, body } => {
                self.write_str("providers {\n");
                self.push_indent();
                for b in bindings {
                    self.write_indent();
                    self.write_ident(&b.resource);
                    self.write_str(" => ");
                    self.format_expr(&b.value);
                    self.write_str(",\n");
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("} in ");
                self.format_block(body);
            }
            ExprKind::OffsetOf { ty, field_path } => {
                self.write_str("offset_of[");
                self.format_type_expr(ty);
                self.write_str("](");
                for (i, segment) in field_path.iter().enumerate() {
                    if i > 0 {
                        self.write_str(".");
                    }
                    self.write_ident(segment);
                }
                self.write_str(")");
            }
            ExprKind::Error => self.write_str("/* error */"),
        }
    }

    pub(super) fn format_call_args(&mut self, args: &[CallArg]) {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            if let Some(ref label) = arg.label {
                self.write_ident(label);
                self.write_str(": ");
            }
            if arg.mut_marker {
                self.write_str("mut ");
            }
            self.format_expr(&arg.value);
        }
    }
}
