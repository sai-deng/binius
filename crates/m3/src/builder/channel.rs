// Copyright 2025 Irreducible Inc.

use binius_core::constraint_system::channel::{ChannelId, FlushDirection};

use super::column::ColumnIndex;
use crate::builder::{Col, B1};

/// A flushing rule within a table.
#[derive(Debug)]
pub struct Flush {
	pub column_indices: Vec<ColumnIndex>,
	pub channel_id: ChannelId,
	pub direction: FlushDirection,
	/// The number of times the values are flushed to the channel.
	pub multiplicity: u32,
	/// Selector columns that determine which row events are flushed
	///
	/// The referenced selector columns must hold 1-bit values.
	pub selectors: Vec<ColumnIndex>,
}

/// Options for a channel flush.
#[derive(Debug)]
pub struct FlushOpts {
	/// The number of times the values are flushed to the channel.
	pub multiplicity: u32,
	/// Selector columns that determine which row events are flushed..
	///
	/// The referenced selector columns must hold 1-bit values and contain only zeros after the
	/// index that is the height of the table. If the selectors is empty, all values up to the
	/// table height are flushed.
	pub selectors: Vec<Col<B1>>,
}

impl Default for FlushOpts {
	fn default() -> Self {
		Self {
			multiplicity: 1,
			selectors: vec![],
		}
	}
}

/// A channel.
#[derive(Debug)]
pub struct Channel {
	pub name: String,
}
