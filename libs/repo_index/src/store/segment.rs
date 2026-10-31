//! The segment file format: header, section table, strings, chunks, sorted terms with
//! delta-varint postings and per-field term frequencies, field lengths, statistics, the key
//! table (paths, packages, commits, blobs, identifiers: [`crate::SegmentRead::chunks_with`]),
//! edges and their reverse index ([`crate::SegmentRead::cited_by`]), crate cards, notes record
//! text ([`crate::SegmentRead::record_text`]), and a checksum footer.
