//! [`crate::SegmentSink`]: builds a segment in a tmp file, fsyncs it and renames it to its
//! content name; bases are built once under an `O_EXCL` lock on their key.
