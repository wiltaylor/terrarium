// Parse Rust, not substrings: whitespace, comments and raw strings must not
// change which router operations the inventory sees.
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::Visit;

fn link_compact(tokens: impl ToTokens) -> String {
    tokens.to_token_stream().to_string().chars().filter(|c| !c.is_whitespace()).collect()
}

fn link_production_source(source: &str) -> String {
    struct Tests(Vec<std::ops::Range<usize>>);
    impl<'ast> Visit<'ast> for Tests {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            let attrs = match item {
                syn::Item::Fn(i) => &i.attrs,
                syn::Item::Mod(i) => &i.attrs,
                syn::Item::Impl(i) => &i.attrs,
                syn::Item::Const(i) => &i.attrs,
                syn::Item::Static(i) => &i.attrs,
                syn::Item::Struct(i) => &i.attrs,
                syn::Item::Enum(i) => &i.attrs,
                syn::Item::Use(i) => &i.attrs,
                syn::Item::Trait(i) => &i.attrs,
                syn::Item::Type(i) => &i.attrs,
                syn::Item::Macro(i) => &i.attrs,
                _ => { syn::visit::visit_item(self, item); return; }
            };
            if attrs.iter().any(|a| link_compact(&a.meta) == "cfg(test)") {
                self.0.push(item.span().byte_range());
            } else {
                syn::visit::visit_item(self, item);
            }
        }
    }
    let file = syn::parse_file(source).expect("inventory must parse every Rust source");
    let mut tests = Tests(Vec::new());
    tests.visit_file(&file);
    let mut bytes = source.as_bytes().to_vec();
    for range in tests.0 {
        for byte in &mut bytes[range] {
            if *byte != b'\n' { *byte = b' '; }
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn link_syntax_inventory(source: &str) -> (Vec<String>, Vec<String>) {
    #[derive(Default)]
    struct Routes { registrations: Vec<String>, compositions: Vec<String> }
    fn inline_methods(expr: &syn::Expr) -> bool {
        match expr {
            syn::Expr::Call(call) => matches!(&*call.func, syn::Expr::Path(path)
                if path.path.segments.last().is_some_and(|s| matches!(s.ident.to_string().as_str(),
                    "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace" | "any" | "on"))),
            syn::Expr::MethodCall(call) => inline_methods(&call.receiver),
            _ => false,
        }
    }
    impl<'ast> Visit<'ast> for Routes {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            match call.method.to_string().as_str() {
                "route" => {
                    assert_eq!(call.args.len(), 2, "unsupported route arguments");
                    assert!(inline_methods(&call.args[1]), "method-router variables must be classified structurally before use");
                    self.registrations.push(link_compact(&call.args));
                }
                "merge" | "nest" => {
                    self.compositions.push(format!("{}({})", call.method, link_compact(&call.args)));
                }
                "route_service" | "nest_service" => panic!("service routes need explicit inventory support"),
                _ => {}
            }
            syn::visit::visit_expr_method_call(self, call);
        }
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = &*call.func {
                assert!(!path.path.segments.last().is_some_and(|s| matches!(s.ident.to_string().as_str(),
                    "route" | "merge" | "nest" | "route_service" | "nest_service")),
                    "UFCS router operations need explicit inventory support");
            }
            syn::visit::visit_expr_call(self, call);
        }
        fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
            assert!(!matches!(rename.ident.to_string().as_str(), "Operator" | "operator_only" | "operator_only_with_provenance"),
                "operator gate aliases need explicit inventory support");
            syn::visit::visit_use_rename(self, rename);
        }
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            let tokens = link_compact(&mac.tokens);
            assert!(![".route(", ".merge(", ".nest(", ".route_service(", ".nest_service("].iter().any(|s| tokens.contains(s)),
                "router operations in macros need explicit inventory support");
            syn::visit::visit_macro(self, mac);
        }
    }
    let file = syn::parse_file(source).expect("inventory must parse Rust");
    let mut routes = Routes::default();
    routes.visit_file(&file);
    routes.compositions.sort();
    (routes.registrations, routes.compositions)
}

#[test]
fn link_inventory_parser_regressions() {
    for source in [
        "fn routes() { r.nest_service(\"/x\", service); }",
        "fn routes() { r . route_service (\"/x\", service); }",
        "fn routes() { r.route(\"/x\", methods); methods = methods.post(write); }",
        "fn routes() { Router::route(r, \"/x\", get(read)); }",
        "use crate::Actor::Operator as Root;",
        "fn routes() { register!(r.route(\"/x\", get(read))); }",
    ] {
        assert!(std::panic::catch_unwind(|| link_syntax_inventory(source)).is_err(), "accepted {source}");
    }
    for source in ["fn routes() { r.nest(\"/new\", existing); }", "fn routes() { r.merge(existing); }"] {
        assert!(!link_syntax_inventory(source).1.is_empty());
    }
    let source = link_production_source("#[cfg(test)]\nmod tests { fn test() {} }\nfn routes() { r . route (r#\"/real\"#, get(read).post(write)); }\n");
    assert_eq!(link_syntax_inventory(&source).0, ["r#\"/real\"#,get(read).post(write)"]);
    for spelling in ["Self :: Operator", "Operator", "label == \"operator\"", "policy :: operator_only (actor)"] {
        assert!(!link_gate_lines(spelling).is_empty(), "missed {spelling}");
    }
}
