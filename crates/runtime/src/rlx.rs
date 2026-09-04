//! The declarative layer's pure core, installed as the `rlx` global.
//!
//! `rlx.js` is deliberately engine-only: `h()`/`render()` turn a virtual
//! element tree into a flat list of draw ops with no raylib dependency, so
//! the headless tests below exercise the whole tree→op path on the real
//! engine without a window. `rlx.present()` drives a frame end to end
//! (render, draw every op through `rlx.draw`, dispatch control events), and
//! `rlx.useState`/`rlx.useRef` keep component state alive per tree path
//! across frames, pruning it when a component leaves the tree. `rlx.draw()`
//! is the stock backend that maps ops onto the `rl.gui*` surface and only
//! needs raylib at draw time.

use crux::error::JsError;

use crate::agent::Agent;

/// The `rlx.js` library source, evaluated by [`install`].
const SOURCE: &str = include_str!("rlx.js");

/// Install the `rlx` global on the current realm.
pub fn install(agent: &mut Agent) -> Result<(), JsError> {
    agent.run_script(SOURCE)?;
    agent.run_jobs()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::embed::Context;

    /// Evaluate `source` and require it to complete to boolean `true`.
    fn assert_eval_true(context: &mut Context, source: &str) {
        let value = context.eval(source).unwrap();
        assert_eq!(
            value.as_boolean(),
            Some(true),
            "expected `true` from: {source}"
        );
    }

    /// Like `assert_eval_true`, through the JSX-eval goal.
    fn assert_jsx_true(context: &mut Context, source: &str) {
        let value = context.eval_jsx(source).unwrap();
        assert_eq!(
            value.as_boolean(),
            Some(true),
            "expected `true` from (jsx): {source}"
        );
    }

    #[test]
    fn installs_the_rlx_helpers() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        assert_eval_true(
            &mut context,
            "typeof rlx === 'object' && typeof rlx.h === 'function' && \
             typeof rlx.render === 'function' && typeof rlx.draw === 'function'",
        );
    }

    #[test]
    fn h_normalizes_children_like_react() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // Strings survive, arrays flatten, nothing-shaped values drop, and
        // numbers become text.
        assert_eval_true(
            &mut context,
            "(() => { const n = rlx.h('label', null, 'a', ['b', false, null], 3); \
             return n.children.length === 3 && n.children[0] === 'a' && \
             n.children[1] === 'b' && n.children[2] === '3'; })()",
        );
    }

    #[test]
    fn render_flattens_the_tree_into_ordered_ops_with_paths() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        assert_eval_true(
            &mut context,
            "(() => { const ops = rlx.render(rlx.h('group', { x: 0 }, \
             rlx.h('label', {}, 'Hi'), rlx.h('button', {}, 'Go'))); \
             return ops.length === 3 && ops[0].type === 'group' && ops[0].path === '0' && \
             ops[0].text === '' && ops[1].type === 'label' && ops[1].path === '0/0' && \
             ops[1].text === 'Hi' && ops[2].type === 'button' && ops[2].path === '0/1' && \
             ops[2].text === 'Go'; })()",
        );
    }

    #[test]
    fn render_expands_components_and_fragments() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        assert_eval_true(
            &mut context,
            "(() => { function Row(props) { return rlx.h('group', { y: props.y }, \
             rlx.h('label', {}, props.text)); } \
             const ops = rlx.render([rlx.h(Row, { y: 3, text: 'A' }), rlx.h(Row, { y: 4, text: 'B' })]); \
             return ops.length === 4 && ops[0].type === 'group' && ops[0].props.y === 3 && \
             ops[0].path === '0/0' && ops[1].type === 'label' && ops[1].text === 'A' && \
             ops[1].path === '0/0/0' && ops[2].props.y === 4 && ops[3].text === 'B'; })()",
        );
    }

    #[test]
    fn draw_requires_the_rl_host_module() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // Assert inside JS: a JS-thrown `Error` crossing to Rust formats as a
        // generic object, but `e.message` is intact in the catch block.
        assert_eval_true(
            &mut context,
            "(() => { try { rlx.draw({ type: 'label', props: {}, text: '' }); return false; } \
             catch (error) { return error instanceof Error && error.message.indexOf('rl host module') !== -1; } })()",
        );
    }

    #[test]
    fn eval_jsx_desugars_elements_into_rlx_calls() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // JSX elements become rlx.h(...) calls through the whole
        // parse → compile → run path.
        assert_jsx_true(
            &mut context,
            "(() => { const ops = rlx.render(<button x={10} y={20}>Go</button>); \
             return ops.length === 1 && ops[0].type === 'button' && ops[0].text === 'Go' && \
             ops[0].props.x === 10 && ops[0].props.y === 20 && ops[0].path === '0'; })()",
        );
        // Components and nested elements flow through too.
        assert_jsx_true(
            &mut context,
            "(() => { function Demo() { return <panel>hi<label>nested</label></panel>; } \
             const ops = rlx.render(<Demo/>); \
             return ops.length === 2 && ops[0].text === 'hi' && ops[1].text === 'nested'; })()",
        );
        // The default eval goal still rejects the same text: JSX is opt-in.
        let error = match context.eval("const v = <button/>;") {
            Ok(_) => panic!("plain eval must reject JSX"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("unexpected") || error.contains("Unexpected"),
            "{error}"
        );
    }

    #[test]
    fn jsx_text_children_render_cleaned_text() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // Raw JSXText is scanned (never tokenized), so comment-like text
        // survives, and Babel-style whitespace cleaning applies.
        assert_jsx_true(
            &mut context,
            "rlx.render(<box>50% off // sale</box>)[0].text === '50% off // sale'",
        );
        assert_jsx_true(
            &mut context,
            "rlx.render(<label>say \"hi\"</label>)[0].text === 'say \"hi\"'",
        );
        // Indented lines between elements collapse; in-line spaces survive.
        assert_jsx_true(
            &mut context,
            "(() => { const ops = rlx.render(<box>\n    hello\n    <b>world</b>\n  </box>); \
             return ops[0].text === 'hello' && ops[1].text === 'world'; })()",
        );
        assert_jsx_true(&mut context, "rlx.render(<box>a</box>)[0].text === 'a'");
    }

    #[test]
    fn jsx_fragments_and_spreads_run() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // A fragment (array) flattens into sibling ops.
        assert_jsx_true(
            &mut context,
            "(() => { const ops = rlx.render(<><label>a</label><label>b</label></>); \
             return ops.length === 2 && ops[0].text === 'a' && ops[1].text === 'b'; })()",
        );
        // Spread attributes merge into props.
        assert_jsx_true(
            &mut context,
            "rlx.render(<box {...{x: 1}} y={2}/>)[0].props.x === 1 && \
             rlx.render(<box {...{x: 1}} y={2}/>)[0].props.y === 2",
        );
        // Dashed tags/attributes and namespaced names desugar to strings.
        assert_jsx_true(
            &mut context,
            "(() => { const op = rlx.render(<my-box data-id=\"7\">x</my-box>)[0]; \
             return op.type === 'my-box' && op.props['data-id'] === '7' && op.text === 'x'; })()",
        );
    }

    #[test]
    fn use_state_persists_per_component_path() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // A mock `rl` lets `present` run headless: the first frame the first
        // button is clicked (mock returns true once), the second frame the
        // label must show the incremented state while a sibling counter at
        // another path keeps its own state untouched.
        assert_eval_true(
            &mut context,
            "(() => { let clicks = 1; globalThis.rl = { guiLabel: function () {}, \
             guiButton: function () { return clicks-- > 0; } }; \
             function Counter() { const state = rlx.useState(0); return [ \
             rlx.h('label', { x: 0, y: 0, width: 10, height: 10 }, 'n=' + state[0]), \
             rlx.h('button', { x: 0, y: 10, width: 10, height: 10, \
             onClick: function () { state[1](function (prev) { return prev + 1; }); } }, 'inc') ]; } \
             const tree = function () { return [rlx.h(Counter, {}), rlx.h(Counter, {})]; }; \
             clicks = 1; const first = rlx.present(tree()); \
             clicks = 0; const second = rlx.present(tree()); \
             return first[0].text === 'n=0' && first[2].text === 'n=0' && \
             second[0].text === 'n=1' && second[2].text === 'n=0'; })()",
        );
    }

    #[test]
    fn stateful_controls_dispatch_on_change_even_for_keystrokes() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        // A text box reports keystrokes as value changes with action 0 and
        // only toggles its edit mode when action is nonzero. The component
        // is controlled: it adopts e.value every change and flips edit mode
        // on action.
        assert_eval_true(
            &mut context,
            "(() => { const replies = [ { action: 0, value: 'h' }, { action: 1, value: 'hi' }, \
             { action: 0, value: 'hi' } ]; let index = 0; \
             globalThis.rl = { guiTextBox: function () { return replies[index++]; } }; \
             function Field() { const text = rlx.useState(''); const edit = rlx.useState(false); \
             return rlx.h('textbox', { x: 0, y: 0, width: 100, height: 24, value: text[0], \
             edit: edit[0], onChange: function (event) { text[1](event.value); \
             if (event.action) edit[1](function (prev) { return !prev; }); } }, ''); } \
             const frame1 = rlx.present(rlx.h(Field, {})); \
             const frame2 = rlx.present(rlx.h(Field, {})); \
             const frame3 = rlx.present(rlx.h(Field, {})); \
             return frame1[0].props.value === '' && frame2[0].props.value === 'h' && \
             frame2[0].props.edit === false && frame3[0].props.value === 'hi' && \
             frame3[0].props.edit === true; })()",
        );
    }

    #[test]
    fn state_is_pruned_when_a_component_leaves_the_tree() {
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        assert_eval_true(
            &mut context,
            "(() => { let clicks = 1; globalThis.rl = { guiLabel: function () {}, \
             guiButton: function () { return clicks-- > 0; } }; \
             function Leaf() { const state = rlx.useState(0); return [ \
             rlx.h('label', { x: 0, y: 0, width: 10, height: 10 }, 'v=' + state[0]), \
             rlx.h('button', { x: 0, y: 10, width: 10, height: 10, \
             onClick: function () { state[1](function (prev) { return prev + 1; }); } }, 'inc') ]; } \
             clicks = 1; rlx.present(rlx.h(Leaf, {})); \
             clicks = 0; rlx.present(rlx.h('label', { x: 0, y: 0, width: 10, height: 10 }, 'x')); \
             clicks = 0; const ops = rlx.present(rlx.h(Leaf, {})); \
             return ops[0].text === 'v=0'; })()",
        );
    }
}
