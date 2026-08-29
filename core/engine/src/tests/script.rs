//! Tests for sharing one compiled script between realms.

use crate::{Context, JsValue, Script, Source, js_string};

/// Compiles `source` in a context of its own, the way an embedder holding a
/// template would.
fn compiled(source: &str) -> (Context, Script) {
    let mut context = Context::default();
    let script = Script::parse(Source::from_bytes(source), None, &mut context).expect("parses");
    script.codeblock(&mut context).expect("compiles");
    (context, script)
}

#[test]
fn a_compiled_script_runs_in_more_than_one_realm() {
    let (_template, script) = compiled("globalThis.ran = (globalThis.ran ?? 0) + 1;");

    let mut first = Context::default();
    script
        .bind_to_realm(first.realm().clone())
        .expect("binds")
        .evaluate(&mut first)
        .expect("runs");

    let mut second = Context::default();
    script
        .bind_to_realm(second.realm().clone())
        .expect("binds")
        .evaluate(&mut second)
        .expect("runs");

    // Two, in either realm, would mean the realms were sharing a global object.
    for context in [&mut first, &mut second] {
        assert_eq!(
            context
                .eval(Source::from_bytes("globalThis.ran"))
                .expect("reads"),
            JsValue::from(1)
        );
    }
}

#[test]
fn what_one_realm_does_to_the_shared_code_is_not_visible_to_the_next() {
    // The function is what carries state if anything does: it is one
    // `CodeBlock`, shared by both realms, holding the inline caches that record
    // the shapes it has seen.
    let (_template, script) = compiled(
        "globalThis.read = function (o) { return o.x; };
         globalThis.marker = 'from this realm';",
    );

    let mut first = Context::default();
    script
        .bind_to_realm(first.realm().clone())
        .expect("binds")
        .evaluate(&mut first)
        .expect("runs");
    first
        .eval(Source::from_bytes(
            "globalThis.read({ x: 1 });
             globalThis.read({ y: 0, x: 2 });
             globalThis.marker = 'changed';
             Object.prototype.injected = 'leaked';",
        ))
        .expect("the first realm runs");

    let mut second = Context::default();
    script
        .bind_to_realm(second.realm().clone())
        .expect("binds")
        .evaluate(&mut second)
        .expect("runs");

    assert_eq!(
        second
            .eval(Source::from_bytes("globalThis.marker"))
            .expect("reads"),
        JsValue::from(js_string!("from this realm")),
    );
    assert_eq!(
        second
            .eval(Source::from_bytes("typeof Object.prototype.injected"))
            .expect("reads"),
        JsValue::from(js_string!("undefined")),
    );
    // The shared function still reads correctly in a realm whose objects have
    // shapes the caches have never seen.
    assert_eq!(
        second
            .eval(Source::from_bytes("globalThis.read({ x: 7 })"))
            .expect("reads"),
        JsValue::from(7)
    );
}

#[test]
fn an_uncompiled_script_cannot_be_bound() {
    let mut context = Context::default();
    let script = Script::parse(Source::from_bytes("1 + 1;"), None, &mut context).expect("parses");

    let other = Context::default();
    assert!(
        script.bind_to_realm(other.realm().clone()).is_err(),
        "binding compiled the script against whichever context was nearest, \
         which is the mistake the missing parameter exists to prevent"
    );
}

#[test]
fn a_top_level_lexical_declaration_is_refused() {
    // `let` at the top level becomes a binding in the global *declarative*
    // scope, which the compiler addresses by position in the scope it compiled
    // against. A second realm's scope would not have it.
    for source in ["let a = 1;", "const a = 1;", "class A {}"] {
        let (_template, script) = compiled(source);
        let context = Context::default();
        assert!(
            script.bind_to_realm(context.realm().clone()).is_err(),
            "`{source}` was allowed into a second realm"
        );
    }

    // `var` and `function` are instantiated by name on the global object, so
    // they are portable and must not be caught by the same guard.
    for source in ["var a = 1;", "function a() {}"] {
        let (_template, script) = compiled(source);
        let mut context = Context::default();
        script
            .bind_to_realm(context.realm().clone())
            .unwrap_or_else(|e| panic!("`{source}` was refused: {e}"))
            .evaluate(&mut context)
            .expect("runs");
        assert_eq!(
            context.eval(Source::from_bytes("typeof a")).expect("reads"),
            JsValue::from(js_string!(if source.starts_with("var") {
                "number"
            } else {
                "function"
            })),
        );
    }
}
