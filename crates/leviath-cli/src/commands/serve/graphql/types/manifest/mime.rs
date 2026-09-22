//! The mime rows a blueprint ships with itself.
//!
//! A blueprint that works in a file type the machine has never heard of carries
//! the row that describes it, so installing the blueprint is all it takes.
//!
//! The row itself is [`MimeRow`], the one type every mime row in this schema
//! is, wherever it came from: a blueprint's row and the operator's row are the
//! same thing said in the same place, and `source` names which. Reading a
//! blueprint's `[mime_types]` table into rows is [`MimeRow::from_table`].
//!
//! [`MimeRow`]: super::super::machine::mime::MimeRow
//! [`MimeRow::from_table`]: super::super::machine::mime::MimeRow::from_table
