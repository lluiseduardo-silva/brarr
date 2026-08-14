//! The orchestrator's side of the metadata abstraction.
//!
//! `brarr-core` holds the vocabulary and the trait; the provider crates
//! hold the two implementations. What lives here is everything that
//! needs a pool, a credential or a screen:
//!
//! - [`art`] builds an image URL for a stored path, given who stored it.
//! - [`axis`] turns a catalogue identity into the search keys the tracker
//!   fan-out speaks — and reports what it could not use.
//!
//! The rule these two share is the reason the module exists: **no
//! provider crate's type crosses into `db::` or `web::`.** Before this
//! there were three crossings — an episode-group type in the data layer,
//! another in a route handler, and `brarr_tmdb::image_url` called from
//! four places in `web/routes.rs` — and the "strict boundaries, never
//! collapse layers" rule had no guard behind it.

pub mod art;
pub mod axis;
