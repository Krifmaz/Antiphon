//! Keyboard reach and the screen-reader tree, from one list.
//!
//! iced 0.14 gives only text boxes keyboard focus and exposes nothing to
//! screen readers. So every control the widget helpers build also
//! *registers* itself here while the view is being built: what it is (a
//! button, a switch, a slider...), what it says, and the message pressing
//! or stepping it would send. That list, rebuilt with every frame, is
//! what both halves work from:
//!
//! - **Keyboard.** Tab and Shift+Tab walk the list (only the top layer,
//!   so an open dialog keeps focus inside it), Enter or Space sends the
//!   focused control's message, and the arrow keys step a slider or a
//!   choice. The focused control draws a ring ([`ring`]) so it can be
//!   seen. Focus is remembered by a key made from the control's role and
//!   words, not its position, so it survives the list changing around it.
//! - **Screen readers.** [`tree`] turns the same list into an AccessKit
//!   tree (roles, names, values, checked and selected states, and which
//!   node has focus) that the app hands to the platform adapter; actions
//!   the reader asks for come back as [`crate::Msg::Reader`] and are
//!   carried out exactly as the keyboard would.
//!
//! Two things make a long screen workable from the keyboard: the focused
//! control is marked in the widget tree ([`mark`]) so [`reveal`] can
//! scroll whatever holds it until it is in view, and a message in a
//! conversation is one stop whose hover tools (react, edit, delete) join
//! the list only while focus is on that message ([`group`]).
//!
//! The list lives in a thread local because the helpers that fill it are
//! plain functions deep in the view; iced builds the view and handles
//! updates on one thread, so the list read by an update is always the one
//! on screen.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use accesskit::{Live, NodeId, Toggled, Tree, TreeId, TreeUpdate};
use iced_core::widget::operation::{Operation, Outcome, Scrollable};
use iced_core::widget::Id;
use iced_core::{Background, Border, Color, Length, Rectangle, Vector};
use iced_widget::{container, space, stack};

use crate::{theme, El, Msg};

/// What a registered control is, which decides how keys and readers treat it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Pressed with Enter or Space.
    Button,
    /// An on/off switch; pressing flips it.
    Toggle,
    /// One of a row of tabs, or a choice among tiles; pressing selects it.
    Tab,
    /// A value on a track; the arrows step it.
    Slider,
    /// A text box; focusing it puts the typing cursor in it.
    Input,
    /// A drop-down choice; the arrows pick the next or previous option.
    Choice,
    /// A title over a part of the screen; read, not focused.
    Heading,
    /// A piece of text worth reading, such as a message; read, not focused.
    Text,
    /// A short notice that appeared (a toast); read out when it appears.
    Alert,
    /// A message in a conversation: a stop of its own, whose tools come
    /// after it while it (or one of them) has focus.
    Message,
}

impl Role {
    /// Whether Tab stops on this kind of node.
    pub fn focusable(self) -> bool {
        !matches!(self, Role::Heading | Role::Text | Role::Alert)
    }
}

/// One registered control or piece of text, as the last frame drew it.
#[derive(Clone, Debug)]
pub struct Node {
    /// Stable identity: role, words and which occurrence of those it is.
    pub key: String,
    /// What it is.
    pub role: Role,
    /// What it says: the name a screen reader reads.
    pub label: String,
    /// Its current value, for sliders, choices and text boxes.
    pub value: Option<String>,
    /// On or off, for switches.
    pub checked: Option<bool>,
    /// Whether it is the selected tab or tile.
    pub selected: bool,
    /// Whether it is an error notice, read out at once rather than politely.
    pub urgent: bool,
    /// Which layer it is on: 0 the window, then each dialog or menu over it.
    pub layer: usize,
    /// Heading level (1 is the screen's title), for headings.
    pub level: usize,
    /// The message pressing it sends.
    pub activate: Option<Msg>,
    /// The message stepping it up (or to the next option) sends.
    pub increase: Option<Msg>,
    /// The message stepping it down (or to the previous option) sends.
    pub decrease: Option<Msg>,
    /// The message that opens its right-click menu, for the menu key.
    pub menu: Option<Msg>,
    /// The iced widget id of a text box, so focus can move the cursor there.
    pub input: Option<&'static str>,
    /// The message this node belongs to (the message itself or one of its
    /// tools), set by [`group`].
    pub group: Option<u64>,
}

impl Node {
    /// The AccessKit id for this node: a hash of its key, so the same
    /// control keeps the same id from frame to frame. Ids 0 and 1 are kept
    /// for the root.
    pub fn id(&self) -> NodeId {
        node_id(&self.key)
    }
}

/// The AccessKit id for a node key (see [`Node::id`]).
pub fn node_id(key: &str) -> NodeId {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    NodeId(h.finish().max(2))
}

/// The root window node's id.
pub const ROOT: NodeId = NodeId(1);

/// A control about to be registered: [`Spec::new`], the builder methods,
/// then [`Spec::add`].
#[must_use = "a spec does nothing until it is added"]
pub struct Spec {
    /// The node being built (its key and layer are filled in on add).
    node: Node,
}

impl Spec {
    /// A control of `role` that reads as `label`.
    pub fn new(role: Role, label: impl Into<String>) -> Self {
        Spec {
            node: Node {
                key: String::new(),
                role,
                label: label.into(),
                value: None,
                checked: None,
                selected: false,
                urgent: false,
                layer: 0,
                level: 0,
                activate: None,
                increase: None,
                decrease: None,
                menu: None,
                input: None,
                group: None,
            },
        }
    }

    /// The message pressing it sends. Any message type is accepted so the
    /// generic widget helpers can pass theirs; only this interface's
    /// [`Msg`] is kept, since that is the only one an update can replay.
    pub fn on<M: Clone + 'static>(mut self, msg: Option<&M>) -> Self {
        self.node.activate = msg.and_then(as_msg);
        self
    }

    /// The messages stepping it up and down.
    pub fn steps<M: Clone + 'static>(mut self, up: Option<&M>, down: Option<&M>) -> Self {
        self.node.increase = up.and_then(as_msg);
        self.node.decrease = down.and_then(as_msg);
        self
    }

    /// The message that opens its right-click menu (Shift+F10 or the
    /// menu key opens it from the keyboard).
    pub fn menu<M: Clone + 'static>(mut self, msg: Option<&M>) -> Self {
        self.node.menu = msg.and_then(as_msg);
        self
    }

    /// Its value as words ("50%", "Built-in microphone").
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.node.value = Some(value.into());
        self
    }

    /// Whether it is switched on.
    pub fn checked(mut self, on: bool) -> Self {
        self.node.checked = Some(on);
        self
    }

    /// Whether it is the selected one of its row.
    pub fn selected(mut self, on: bool) -> Self {
        self.node.selected = on;
        self
    }

    /// The iced id of the text box this node stands for.
    pub fn input(mut self, id: &'static str) -> Self {
        self.node.input = Some(id);
        self
    }

    /// The heading level, for [`Role::Heading`].
    pub fn level(mut self, level: usize) -> Self {
        self.node.level = level;
        self
    }

    /// Marks an alert as urgent: read out at once.
    pub fn urgent(mut self, on: bool) -> Self {
        self.node.urgent = on;
        self
    }

    /// Puts the node in this frame's list. True when it has keyboard
    /// focus, so the caller draws the focus ring.
    pub fn add(self) -> bool {
        let mut node = self.node;
        if node.label.trim().is_empty() {
            // Nothing to read and nothing to tell it apart by; a control
            // with no words should be given some by its caller.
            return false;
        }
        FRAME.with(|f| {
            let mut f = f.borrow_mut();
            if f.paused {
                return false;
            }
            let n = f.counts.entry((node.role, node.label.clone())).or_insert(0);
            node.key = format!("{:?}|{}|{}", node.role, node.label, n);
            *n += 1;
            node.layer = f.layer;
            node.group = f.group;
            let focused = node.role.focusable() && f.focus.as_deref() == Some(node.key.as_str());
            f.nodes.push(node);
            focused
        })
    }
}

/// `msg` as this interface's message, when it is one.
fn as_msg<M: Clone + 'static>(msg: &M) -> Option<Msg> {
    (msg as &dyn Any).downcast_ref::<Msg>().cloned()
}

/// The list being built (or last built) and what it needs while building.
#[derive(Default)]
struct Frame {
    /// The registered nodes, in the order the view built them.
    nodes: Vec<Node>,
    /// The focused node's key, as the interface's state has it.
    focus: Option<String>,
    /// The layer nodes added now go on.
    layer: usize,
    /// How many nodes of each role and label are in, for unique keys.
    counts: HashMap<(Role, String), usize>,
    /// While set, nothing registers (see [`pause`]).
    paused: bool,
    /// The message the nodes added now belong to (see [`group`]).
    group: Option<u64>,
}

thread_local! {
    /// The frame on screen (or being built).
    static FRAME: RefCell<Frame> = RefCell::new(Frame::default());
}

/// Starts a new frame: forgets the old list and remembers which key has focus.
pub fn begin(focus: Option<&str>) {
    FRAME.with(|f| *f.borrow_mut() = Frame { focus: focus.map(str::to_string), ..Frame::default() });
}

/// Puts the nodes added from now on in a new layer over the last: a
/// dialog or a menu, which takes the keyboard while it is up.
pub fn layer() {
    FRAME.with(|f| f.borrow_mut().layer += 1);
}

/// Marks the nodes added from now on as belonging to message `id`, or to
/// no message (`None`). Focus on any of them keeps that message's tools
/// in the keyboard's list.
pub fn group(id: Option<u64>) {
    FRAME.with(|f| f.borrow_mut().group = id);
}

/// Stops (`true`) or restarts registering: for controls drawn only under
/// the pointer, which the keyboard reaches another way or not at all.
pub fn pause(on: bool) {
    FRAME.with(|f| f.borrow_mut().paused = on);
}

/// The nodes the last frame registered.
pub fn nodes() -> Vec<Node> {
    FRAME.with(|f| f.borrow().nodes.clone())
}

/// The nodes Tab can stop on: the focusable ones on the top layer, in order.
pub fn tab_stops(nodes: &[Node]) -> Vec<&Node> {
    let top = nodes.iter().filter(|n| n.role.focusable()).map(|n| n.layer).max().unwrap_or(0);
    nodes.iter().filter(|n| n.role.focusable() && n.layer == top).collect()
}

/// The key Tab (`step` = 1) or Shift+Tab (`step` = -1) moves focus to from
/// `from`. From nothing, or from a node no longer on the top layer, Tab
/// starts at the first stop and Shift+Tab at the last; both wrap around.
pub fn next_stop(nodes: &[Node], from: Option<&str>, step: i32) -> Option<String> {
    let stops = tab_stops(nodes);
    if stops.is_empty() {
        return None;
    }
    let len = stops.len() as i32;
    let at = from.and_then(|k| stops.iter().position(|n| n.key == k));
    let i = match at {
        Some(i) => (i as i32 + step).rem_euclid(len),
        None if step < 0 => len - 1,
        None => 0,
    };
    Some(stops[i as usize].key.clone())
}

/// The widget id the focused control is marked with (see [`mark`]).
pub const FOCUS_MARK: &str = "stoop-keyboard-focus";

/// Wraps `content` in a marker [`reveal`] can find when `focused`; leaves
/// it as it is otherwise. The marker is a plain container of the content's
/// own size, so nothing moves.
pub fn mark<'a, M: 'a>(content: impl Into<El<'a, M>>, focused: bool) -> El<'a, M> {
    let content = content.into();
    if focused {
        container(content).id(Id::new(FOCUS_MARK)).into()
    } else {
        content
    }
}

/// An operation that scrolls the scrollables holding the focused control
/// (the [`mark`]ed one, or the text box with id `input`) just far enough
/// to show it, with a little room around it.
pub fn reveal<T>(input: Option<&'static str>) -> impl Operation<T> {
    Reveal { input, pending: None, stack: Vec::new(), done: false }
}

/// A scrollable seen on the way down: where it shows, how far it is
/// scrolled, and its state to scroll.
struct Holder<'s> {
    /// The visible area, in window coordinates.
    bounds: Rectangle,
    /// How far its content is scrolled.
    translation: Vector,
    /// Its state, through which it is scrolled.
    state: &'s mut dyn Scrollable,
}

/// The state of [`reveal`] as it walks the widget tree.
struct Reveal {
    /// The text box to reveal instead of a marker, if focus is in one.
    input: Option<&'static str>,
    /// A scrollable just met whose content is walked next: (bounds,
    /// translation, its state as a raw pointer, valid until that walk ends).
    pending: Option<(Rectangle, Vector, *mut dyn Scrollable)>,
    /// The scrollables around the widget being visited, outermost first.
    stack: Vec<(Rectangle, Vector, *mut dyn Scrollable)>,
    /// Set once the target was found and shown.
    done: bool,
}

// SAFETY: the raw pointers are only held during one synchronous walk of
// the tree, on the thread doing it; `Operation` asks for `Send` only so an
// operation can be built on one thread and run on another before it starts.
unsafe impl Send for Reveal {}

/// How far to scroll one axis so `[start, end)` shows inside a view of
/// `[view, view + len)` already scrolled by `offset`: `None` when it is in
/// view already. `margin` is the room left around it.
fn scroll_axis(start: f32, end: f32, view: f32, len: f32, offset: f32, margin: f32) -> Option<f32> {
    let (top, bottom) = (view + offset, view + offset + len);
    if start - margin < top {
        Some((start - margin - view).max(0.0))
    } else if end + margin > bottom {
        Some((end + margin - view - len).max(0.0))
    } else {
        None
    }
}

impl Reveal {
    /// The target is at `target` (untranslated layout coordinates): scroll
    /// the innermost scrollable holding it.
    fn show(&mut self, target: Rectangle) {
        if self.done {
            return;
        }
        self.done = true;
        let Some(&(bounds, translation, state)) = self.stack.last() else { return };
        let holder = Holder {
            bounds,
            translation,
            // SAFETY: the scrollable is an ancestor of the widget being
            // visited, so its state outlives this call (see `pending`).
            state: unsafe { &mut *state },
        };
        let y = scroll_axis(target.y, target.y + target.height, holder.bounds.y, holder.bounds.height, holder.translation.y, 12.0);
        let x = scroll_axis(target.x, target.x + target.width, holder.bounds.x, holder.bounds.width, holder.translation.x, 12.0);
        if x.is_some() || y.is_some() {
            holder.state.scroll_to(iced_core::widget::operation::scrollable::AbsoluteOffset { x, y });
        }
    }
}

impl<T> Operation<T> for Reveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        if self.done {
            return;
        }
        // A scrollable walks its content right after reporting itself.
        let held = self.pending.take();
        if let Some(h) = held {
            self.stack.push(h);
        }
        operate(self);
        if held.is_some() {
            self.stack.pop();
        }
    }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if self.input.is_none() && id == Some(&Id::new(FOCUS_MARK)) {
            self.show(bounds);
        }
    }

    fn scrollable(&mut self, _id: Option<&Id>, bounds: Rectangle, _content: Rectangle, translation: Vector, state: &mut dyn Scrollable) {
        // SAFETY: only dereferenced while this scrollable's content is
        // being walked, which `traverse` brackets.
        let state: *mut (dyn Scrollable + '_) = state;
        self.pending = Some((bounds, translation, unsafe { std::mem::transmute::<*mut (dyn Scrollable + '_), *mut dyn Scrollable>(state) }));
    }

    fn text_input(&mut self, id: Option<&Id>, bounds: Rectangle, _state: &mut dyn iced_core::widget::operation::TextInput) {
        if let (Some(want), Some(id)) = (self.input, id) {
            if id == &Id::new(want) {
                self.show(bounds);
            }
        }
    }

    fn finish(&self) -> Outcome<T> {
        Outcome::None
    }
}

/// Draws a focus ring around `content` when `focused`: a two-pixel line in
/// the second accent colour, laid over it so the layout doesn't move.
pub fn ring<'a, M: 'a>(content: impl Into<El<'a, M>>, focused: bool, radius: f32) -> El<'a, M> {
    if !focused {
        return content.into();
    }
    let p = theme::look();
    stack![
        mark(content, true),
        container(space()).width(Length::Fill).height(Length::Fill).style(move |_| container::Style {
            background: Some(Background::Color(Color::TRANSPARENT)),
            border: Border { color: p.accent2, width: 2.0, radius: radius.into() },
            ..container::Style::default()
        }),
    ]
    .into()
}

/// The last frame as an AccessKit tree: a window named `title` holding
/// every registered node in order, with focus where the keyboard's is.
pub fn tree(title: &str) -> TreeUpdate {
    FRAME.with(|f| {
        let f = f.borrow();
        let mut root = accesskit::Node::new(accesskit::Role::Window);
        root.set_label(title.to_string());
        let mut out = Vec::with_capacity(f.nodes.len() + 1);
        let mut children = Vec::with_capacity(f.nodes.len());
        let mut focus = ROOT;
        for n in &f.nodes {
            let id = n.id();
            children.push(id);
            if f.focus.as_deref() == Some(n.key.as_str()) {
                focus = id;
            }
            out.push((id, to_accesskit(n)));
        }
        root.set_children(children);
        out.insert(0, (ROOT, root));
        let mut tree = Tree::new(ROOT);
        tree.toolkit_name = Some("iced".into());
        tree.toolkit_version = Some("0.14".into());
        TreeUpdate { nodes: out, tree: Some(tree), tree_id: TreeId::ROOT, focus }
    })
}

/// One registered node as an AccessKit node.
fn to_accesskit(n: &Node) -> accesskit::Node {
    use accesskit::{Action, Role as R};
    let role = match n.role {
        Role::Button => R::Button,
        Role::Toggle => R::Switch,
        Role::Tab => R::Tab,
        Role::Slider => R::Slider,
        Role::Input => R::TextInput,
        Role::Choice => R::ComboBox,
        Role::Heading => R::Heading,
        Role::Text => R::Label,
        Role::Alert => R::Alert,
        Role::Message => R::ListItem,
    };
    let mut node = accesskit::Node::new(role);
    node.set_label(n.label.clone());
    if let Some(v) = &n.value {
        node.set_value(v.clone());
    }
    if let Some(on) = n.checked {
        node.set_toggled(if on { Toggled::True } else { Toggled::False });
    }
    if n.role == Role::Tab {
        node.set_selected(n.selected);
    }
    if n.role == Role::Heading && n.level > 0 {
        node.set_level(n.level);
    }
    if n.role == Role::Alert {
        node.set_live(if n.urgent { Live::Assertive } else { Live::Polite });
    }
    if n.role.focusable() {
        node.add_action(Action::Focus);
    }
    if n.activate.is_some() {
        node.add_action(Action::Click);
    }
    if n.increase.is_some() {
        node.add_action(Action::Increment);
    }
    if n.decrease.is_some() {
        node.add_action(Action::Decrement);
    }
    if n.menu.is_some() {
        node.add_action(Action::ShowContextMenu);
    }
    node
}

/// What a screen reader asked to be done to a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderAction {
    /// Move keyboard focus to it.
    Focus,
    /// Press it.
    Click,
    /// Step it up.
    Increase,
    /// Step it down.
    Decrease,
    /// Open its right-click menu.
    Menu,
}

impl ReaderAction {
    /// The reader action for an AccessKit action, if it is one this
    /// interface supports.
    pub fn from_accesskit(action: accesskit::Action) -> Option<Self> {
        match action {
            accesskit::Action::Focus => Some(ReaderAction::Focus),
            accesskit::Action::Click => Some(ReaderAction::Click),
            accesskit::Action::Increment => Some(ReaderAction::Increase),
            accesskit::Action::Decrement => Some(ReaderAction::Decrease),
            accesskit::Action::ShowContextMenu => Some(ReaderAction::Menu),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    /// Registers a button that sends `Do(Copy(label))`, returning whether
    /// it has focus.
    fn button(label: &str) -> bool {
        let msg = Msg::Do(Action::Copy(label.into()));
        Spec::new(Role::Button, label).on(Some(&msg)).add()
    }

    #[test]
    fn tab_walks_the_top_layer_and_wraps() {
        begin(None);
        button("a");
        button("b");
        Spec::new(Role::Heading, "Title").add();
        let nodes = nodes();
        let first = next_stop(&nodes, None, 1).unwrap();
        assert!(first.contains("|a|"));
        let second = next_stop(&nodes, Some(&first), 1).unwrap();
        assert!(second.contains("|b|"), "headings are read, not tabbed to");
        assert_eq!(next_stop(&nodes, Some(&second), 1).unwrap(), first, "wraps to the start");
        assert_eq!(next_stop(&nodes, None, -1).unwrap(), second, "Shift+Tab from nothing starts at the end");
    }

    #[test]
    fn a_dialog_keeps_focus_inside() {
        begin(None);
        button("behind");
        layer();
        button("ok");
        button("cancel");
        let stops: Vec<_> = tab_stops(&nodes()).iter().map(|n| n.label.clone()).collect();
        assert_eq!(stops, ["ok", "cancel"]);
    }

    #[test]
    fn focus_follows_the_key_and_same_words_stay_apart() {
        begin(None);
        button("Call");
        button("Call");
        let second = nodes()[1].key.clone();
        assert_ne!(nodes()[0].key, second);
        begin(Some(&second));
        assert!(!button("Call"));
        assert!(button("Call"), "the second one has focus again in the next frame");
    }

    #[test]
    fn the_tree_names_every_node_and_marks_focus() {
        begin(None);
        button("Settings");
        Spec::new(Role::Toggle, "Reduce motion").checked(true).add();
        let key = nodes()[1].key.clone();
        begin(Some(&key));
        button("Settings");
        Spec::new(Role::Toggle, "Reduce motion").checked(true).add();
        let t = tree("Stoop");
        assert_eq!(t.nodes.len(), 3);
        assert_eq!(t.nodes[0].1.label(), Some("Stoop"));
        assert_eq!(t.nodes[2].1.role(), accesskit::Role::Switch);
        assert_eq!(t.nodes[2].1.toggled(), Some(Toggled::True));
        assert_eq!(t.focus, t.nodes[2].0);
    }

    #[test]
    fn scrolling_moves_just_enough_to_show_it() {
        // A view of 0..100, not scrolled: something at 150..170 needs 82.
        assert_eq!(scroll_axis(150.0, 170.0, 0.0, 100.0, 0.0, 12.0), Some(82.0));
        // Already in view: no scroll.
        assert_eq!(scroll_axis(40.0, 60.0, 0.0, 100.0, 0.0, 12.0), None);
        // Scrolled past it: back up to just above it, never below zero.
        assert_eq!(scroll_axis(20.0, 40.0, 0.0, 100.0, 200.0, 12.0), Some(8.0));
        assert_eq!(scroll_axis(5.0, 20.0, 0.0, 100.0, 200.0, 12.0), Some(0.0));
    }

    #[test]
    fn nodes_carry_their_message() {
        begin(None);
        group(Some(7));
        Spec::new(Role::Message, "You: hi").add();
        button("Edit");
        group(None);
        button("Send");
        let n = nodes();
        assert_eq!((n[0].group, n[1].group, n[2].group), (Some(7), Some(7), None));
    }

    #[test]
    fn controls_without_words_are_left_out() {
        begin(None);
        assert!(!button("  "));
        assert!(nodes().is_empty());
    }
}
