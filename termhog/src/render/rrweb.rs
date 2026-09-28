//! rrweb's formats: the event, source and node type numbers, and the DOM
//! changes a Mutation event carries, which are typed, since they make up
//! most of what a recording sends.

use serde::Serialize;
use serde_json::Value;

// EventType
pub const FULL_SNAPSHOT: i64 = 2;
pub const INCREMENTAL_SNAPSHOT: i64 = 3;
pub const META: i64 = 4;
// IncrementalSource
pub const MUTATION: i64 = 0;
pub const MOUSE_INTERACTION: i64 = 2;
// MouseInteractions
pub const MOUSE_UP: i64 = 0;
pub const TOUCH_START: i64 = 7;
// PointerTypes: Mouse=0, Pen=1, Touch=2
pub const POINTER_TOUCH: i64 = 2;
// rrweb-snapshot NodeType
pub const NODE_DOCUMENT: i64 = 0;
pub const NODE_ELEMENT: i64 = 2;
pub const NODE_TEXT: i64 = 3;

/// One rrweb event.
pub enum Event {
    /// A Mutation event: `mutation` applied at `timestamp`.
    Mutation { mutation: Mutation, timestamp: u64 },
    /// Any other event, as its JSON.
    Other(Value),
}

/// The DOM changes one Mutation event makes.
#[derive(Default, Serialize)]
pub struct Mutation {
    pub texts: Vec<TextChange>,
    pub attributes: Vec<AttributeChange>,
    pub removes: Vec<Remove>,
    pub adds: Vec<Add>,
}

impl Mutation {
    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
            && self.attributes.is_empty()
            && self.removes.is_empty()
            && self.adds.is_empty()
    }
}

/// Replace text node `id`'s content.
#[derive(Serialize)]
pub struct TextChange {
    pub id: i64,
    pub value: String,
}

/// Set element `id`'s inline style, or remove it with `None`.
#[derive(Serialize)]
pub struct AttributeChange {
    pub id: i64,
    pub attributes: StyleChange,
}

#[derive(Serialize)]
pub struct StyleChange {
    pub style: Option<String>,
}

/// Remove node `id` from `parent_id`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Remove {
    pub parent_id: i64,
    pub id: i64,
}

/// Append `node` as `parent_id`'s last child. The replayer builds an added
/// node without its children, so each child arrives as an add of its own,
/// after its parent.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Add {
    pub parent_id: i64,
    next_id: Option<i64>,
    pub node: Node,
}

impl Add {
    pub fn new(parent_id: i64, node: Node) -> Add {
        Add {
            parent_id,
            next_id: None,
            node,
        }
    }
}

/// A node, as rrweb-snapshot serializes it, without children.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Node {
    Element(Element),
    Text(Text),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    #[serde(rename = "type")]
    kind: i64,
    tag_name: &'static str,
    attributes: Attributes,
    id: i64,
    /// Always empty: children arrive as adds of their own.
    child_nodes: [(); 0],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Text {
    #[serde(rename = "type")]
    kind: i64,
    text_content: String,
    id: i64,
}

/// An element's attributes, each left out when unset.
#[derive(Default, Serialize)]
pub struct Attributes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<&'static str>,
}

impl Node {
    pub fn element(tag_name: &'static str, id: i64, attributes: Attributes) -> Node {
        Node::Element(Element {
            kind: NODE_ELEMENT,
            tag_name,
            attributes,
            id,
            child_nodes: [],
        })
    }

    pub fn text(id: i64, text_content: String) -> Node {
        Node::Text(Text {
            kind: NODE_TEXT,
            text_content,
            id,
        })
    }
}

impl Attributes {
    /// Inline `css`, if any.
    pub fn style(css: &str) -> Attributes {
        Attributes {
            style: (!css.is_empty()).then(|| css.to_string()),
            class: None,
        }
    }
}

#[cfg(test)]
impl Event {
    /// The event's JSON, uncompressed.
    pub fn to_json(&self) -> Value {
        match self {
            Event::Mutation {
                mutation,
                timestamp,
            } => {
                let mut data = serde_json::to_value(mutation).unwrap();
                data["source"] = MUTATION.into();
                serde_json::json!({
                    "type": INCREMENTAL_SNAPSHOT,
                    "data": data,
                    "timestamp": timestamp,
                })
            }
            Event::Other(event) => event.clone(),
        }
    }
}
