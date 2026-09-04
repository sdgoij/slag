// rlx.js — declarative-layer core (slice 1).
//
// A virtual-element layer over the immediate-mode raygui surface: describe a
// tree with h(), render it to a flat list of draw ops, then hand the ops to
// a backend. rlx.draw() is the stock raygui backend (it needs the `rl`
// global at draw time); tests and custom hosts replace it with a recorder to
// capture the ops without a window.
//
// Slice 1 adds retained state and events on top of the slice-0 tree→ops
// path:
//
//   - Components own state across frames with rlx.useState(initial) /
//     rlx.useRef(initial), stored per component *path* in the tree. A
//     component's state survives as long as it renders at the same path and
//     is dropped (pruned) the first frame it no longer appears. Children
//     with a `key` prop use the key in their path, so lists can keep
//     identity across reorders.
//   - rlx.present(tree) is the per-frame driver: render, draw every op, and
//     dispatch the control's result to an event prop. Presses (button,
//     window) fire `onClick`; stateful controls (checkbox, toggle, slider,
//     textbox) fire `onChange` when the user activated them OR the returned
//     value differs from the value they were given (text input reports
//     keystrokes with no action flag). Stateful controls are *controlled*:
//     feed them from useState and adopt the update in onChange.
//
// Nothing here touches `rl` at definition time, so the module installs on
// any realm; rlx.present() calls whatever rlx.draw currently is, so hosts
// can layer custom ops (canvas, sprites) over the stock control backend.

(function (global) {
    "use strict";

    // React-ish content model: drop nothing-shaped values, flatten nested
    // arrays, and coerce numbers to text.
    function flatten(out, children) {
        for (let i = 0; i < children.length; i++) {
            const child = children[i];
            if (child === null || child === undefined || typeof child === "boolean") continue;
            if (Array.isArray(child)) {
                flatten(out, child);
            } else if (typeof child === "number") {
                out.push(String(child));
            } else {
                out.push(child);
            }
        }
    }

    // h("button", { x: 10, y: 10, width: 100, height: 24 }, "Click") -> node.
    // Component types are functions; control types are plain strings that a
    // backend knows how to draw.
    function h(type, props, ...children) {
        const flat = [];
        flatten(flat, children);
        return { type: type, props: props || {}, children: flat };
    }

    // ---- retained state (slice 1) ----

    // Component path -> { slots: [] }. `useState`/`useRef` read/write slots
    // of the component currently rendering; `visited` tracks which paths
    // appeared this frame so dead state can be pruned after the walk.
    const store = new Map();
    const visited = new Set();
    let frame = null; // { slots, cursor } of the component being rendered

    function useState(initial) {
        if (frame === null) {
            throw new Error("rlx.useState must be called while rendering a component");
        }
        const slots = frame.slots;
        const index = frame.cursor++;
        if (index >= slots.length) slots.push(initial);
        return [
            slots[index],
            function set(next) {
                slots[index] = typeof next === "function" ? next(slots[index]) : next;
            },
        ];
    }

    // A stable mutable box, e.g. for physics state advanced every frame.
    function useRef(initial) {
        if (frame === null) {
            throw new Error("rlx.useRef must be called while rendering a component");
        }
        const slots = frame.slots;
        const index = frame.cursor++;
        if (index >= slots.length) slots.push({ current: initial });
        return slots[index];
    }

    // Resolve components (function types) and flatten fragments (arrays)
    // into a flat list of ops in draw order. Each op carries its `path` in
    // the tree ("0/0/1") — the stable identity state is stored against —
    // plus the joined string content of its text children. Children with a
    // `key` prop take the key as their path segment.
    function identity(child, index) {
        const keyed =
            child !== null &&
            typeof child === "object" &&
            !Array.isArray(child) &&
            child.props !== undefined &&
            child.props.key !== undefined;
        return keyed ? String(child.props.key) : String(index);
    }

    function render(root) {
        const ops = [];
        visited.clear();
        function walk(node, key) {
            if (node === null || node === undefined || typeof node === "string") return;
            if (Array.isArray(node)) {
                for (let i = 0; i < node.length; i++) {
                    walk(node[i], key + "/" + identity(node[i], i));
                }
                return;
            }
            if (typeof node.type === "function") {
                const props = Object.assign({}, node.props);
                if (node.children.length === 1) props.children = node.children[0];
                else if (node.children.length > 1) props.children = node.children;
                const previous = frame;
                let entry = store.get(key);
                if (entry === undefined) {
                    entry = { slots: [] };
                    store.set(key, entry);
                }
                visited.add(key);
                frame = { slots: entry.slots, cursor: 0 };
                let output;
                try {
                    output = node.type(props);
                } finally {
                    frame = previous;
                }
                walk(output, key);
                return;
            }
            const text = [];
            const elements = [];
            for (let i = 0; i < node.children.length; i++) {
                if (typeof node.children[i] === "string") text.push(node.children[i]);
                else elements.push(node.children[i]);
            }
            ops.push({ path: key, type: node.type, props: node.props, text: text.join("") });
            for (let i = 0; i < elements.length; i++) {
                walk(elements[i], key + "/" + identity(elements[i], i));
            }
        }
        walk(root, "0");
        // Drop state for components absent from this frame's tree.
        for (const path of store.keys()) {
            if (!visited.has(path)) store.delete(path);
        }
        return ops;
    }

    // ---- the stock raygui backend ----

    // Geometry comes from props, text from the op's string children. Return
    // values pass through untouched so the dispatcher can react to them.
    function coord(props, name) {
        return props[name] === undefined ? 0 : props[name];
    }
    function label(props, name) {
        const value = props[name];
        return value === undefined ? "" : String(value);
    }
    function number(props, name, fallback) {
        const value = Number(props[name]);
        return Number.isFinite(value) ? value : fallback;
    }

    function draw(op) {
        const rl = global.rl;
        if (!rl) {
            throw new Error(
                "rlx.draw: the rl host module is not installed (build with the raylib feature)",
            );
        }
        const x = coord(op.props, "x");
        const y = coord(op.props, "y");
        const w = coord(op.props, "width");
        const h = coord(op.props, "height");
        switch (op.type) {
            case "panel":
                rl.guiPanel(x, y, w, h, op.text);
                return undefined;
            case "group":
                rl.guiGroupBox(x, y, w, h, op.text);
                return undefined;
            case "label":
                rl.guiLabel(x, y, w, h, op.text);
                return undefined;
            case "status":
                rl.guiStatusBar(x, y, w, h, op.text);
                return undefined;
            case "window":
                return rl.guiWindowBox(x, y, w, h, op.text);
            case "button":
                return rl.guiButton(x, y, w, h, op.text);
            case "checkbox":
                return rl.guiCheckBox(x, y, w, h, op.text, op.props.checked === true);
            case "toggle":
                return rl.guiToggle(x, y, w, h, op.text, op.props.active === true);
            case "textbox":
                return rl.guiTextBox(
                    x,
                    y,
                    w,
                    h,
                    op.props.value === undefined ? "" : String(op.props.value),
                    op.props.edit === true,
                );
            case "slider":
                return rl.guiSlider(
                    x,
                    y,
                    w,
                    h,
                    label(op.props, "left"),
                    label(op.props, "right"),
                    number(op.props, "value", 0),
                    number(op.props, "min", 0),
                    number(op.props, "max", 100),
                );
            case "progress":
                return rl.guiProgressBar(
                    x,
                    y,
                    w,
                    h,
                    label(op.props, "left"),
                    label(op.props, "right"),
                    number(op.props, "value", 0),
                    number(op.props, "min", 0),
                    number(op.props, "max", 1),
                );
            default:
                throw new Error("rlx.draw: unknown control '" + op.type + "'");
        }
    }

    // ---- events (slice 1) ----

    // Types whose draw result is a press: fire the named prop when true.
    const PRESS_PROPS = { window: "onClick", button: "onClick" };
    // Types whose draw result is a { action, value } state update.
    const STATE_PROPS = {
        checkbox: "onChange",
        toggle: "onChange",
        slider: "onChange",
        textbox: "onChange",
    };

    // The value an op was drawn with, so the dispatcher can tell "the user
    // changed it" from "same as last frame". Kept in sync with the defaults
    // the stock draw() feeds each control.
    function inputValue(op) {
        const props = op.props;
        switch (op.type) {
            case "checkbox":
                return props.checked === true;
            case "toggle":
                return props.active === true;
            case "slider":
                return number(props, "value", 0);
            case "textbox":
                return props.value === undefined ? "" : String(props.value);
        }
        return undefined;
    }

    function dispatch(op, result) {
        const press = PRESS_PROPS[op.type];
        if (press !== undefined) {
            if (result === true && typeof op.props[press] === "function") {
                op.props[press](result);
            }
            return;
        }
        const state = STATE_PROPS[op.type];
        if (state === undefined || typeof op.props[state] !== "function") return;
        const action =
            result !== null && typeof result === "object" && "action" in result
                ? result.action
                : 0;
        const changed =
            result !== null &&
            typeof result === "object" &&
            result.value !== inputValue(op);
        if (action !== 0 || changed) op.props[state](result);
    }

    // The per-frame driver: render the tree, draw every op through the
    // current rlx.draw, and dispatch events. Returns the frame's ops.
    function present(tree) {
        const ops = render(tree);
        for (let i = 0; i < ops.length; i++) {
            dispatch(ops[i], global.rlx.draw(ops[i]));
        }
        return ops;
    }

    global.rlx = {
        h: h,
        render: render,
        draw: draw,
        present: present,
        useState: useState,
        useRef: useRef,
    };
})(typeof globalThis !== "undefined" ? globalThis : this);
