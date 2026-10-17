//! Today's page sets written as items of the data format
//! ([`plumb_format`]), so a set can be published in it without being
//! rebuilt: [`plumb_index::page_items`], kept next to the reading back of
//! its items ([`plumb_index::items::page_of_item`]).

pub use plumb_index::page_items::{
    differing_members, kind_of, page_fields, page_item, page_item_or_why, visible,
};
