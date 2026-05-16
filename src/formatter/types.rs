//! Type-expression and generics printing for the canonical formatter.

use crate::ast::*;

impl super::Formatter {
    pub(super) fn format_generic_params(&mut self, gp: &Option<GenericParams>) {
        let gp = match gp {
            Some(g) => g,
            None => return,
        };
        self.write_str("[");
        let mut first = true;
        for p in &gp.params {
            if !first {
                self.write_str(", ");
            }
            first = false;
            if p.is_const {
                self.write_str("const ");
            }
            self.write_ident(&p.name);
            if let Some(ref ct) = p.const_type {
                self.write_str(": ");
                self.format_type_expr(ct);
            } else if !p.bounds.is_empty() {
                self.write_str(": ");
                for (i, b) in p.bounds.iter().enumerate() {
                    if i > 0 {
                        self.write_str(" + ");
                    }
                    self.write_path(&b.path);
                    self.format_generic_args_opt(&b.generic_args);
                }
            }
        }
        for ep in &gp.effect_params {
            if !first {
                self.write_str(", ");
            }
            first = false;
            self.write_str("effect ");
            self.write_ident(ep);
        }
        self.write_str("]");
    }

    pub(super) fn format_generic_args_opt(&mut self, args: &Option<Vec<GenericArg>>) {
        let args = match args {
            Some(a) => a,
            None => return,
        };
        self.write_str("[");
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            match arg {
                GenericArg::Type(t) => self.format_type_expr(t),
                GenericArg::Const(e) => self.format_expr(e),
            }
        }
        self.write_str("]");
    }

    // ── Types ───────────────────────────────────────────────────

    pub(super) fn format_type_expr(&mut self, ty: &TypeExpr) {
        match &ty.kind {
            TypeKind::Path(p) => {
                self.write_path(&p.segments);
                self.format_generic_args_opt(&p.generic_args);
            }
            TypeKind::Tuple(types) => {
                self.write_str("(");
                for (i, t) in types.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_type_expr(t);
                }
                self.write_str(")");
            }
            TypeKind::Array { element, size } => {
                self.write_str("[");
                self.format_type_expr(element);
                self.write_str("; ");
                self.format_expr(size);
                self.write_str("]");
            }
            TypeKind::Pointer { is_mut, inner } => {
                if *is_mut {
                    self.write_str("*mut ");
                } else {
                    self.write_str("*");
                }
                self.format_type_expr(inner);
            }
            TypeKind::FnType {
                params,
                return_type,
                ..
            } => {
                self.write_str("fn(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_type_expr(p);
                }
                self.write_str(")");
                if let Some(ref rt) = return_type {
                    self.write_str(" -> ");
                    self.format_type_expr(rt);
                }
            }
            TypeKind::Ref(inner) => {
                self.write_str("ref ");
                self.format_type_expr(inner);
            }
            TypeKind::MutRef(inner) => {
                self.write_str("mut ref ");
                self.format_type_expr(inner);
            }
            TypeKind::MutSlice(element) => {
                self.write_str("mut Slice[");
                self.format_type_expr(element);
                self.write_str("]");
            }
            TypeKind::Weak(inner) => {
                self.write_str("weak ");
                self.format_type_expr(inner);
            }
            TypeKind::Unit => self.write_str("()"),
            TypeKind::Error => self.write_str("/* error */"),
        }
    }
}
