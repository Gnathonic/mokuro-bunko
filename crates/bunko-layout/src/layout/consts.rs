//! Every threshold of `line_layout.py:84-303`, in ems of the line's thickness.
//! The evidence behind each number is in the Python source's comments.

pub const AMBIGUOUS_ASPECT: f64 = 1.3;
pub const VOTE_ASPECT: f64 = 1.1;
pub const NEIGHBOUR_REACH_EM: f64 = 3.0;
pub const ANGLE_RELIABLE_ASPECT: f64 = 3.0;
pub const SETTLE_MAX_TILT_DEG: f64 = 12.0;
pub const ANGLE_TOLERANCE_DEG: f64 = 6.0;

pub const FURIGANA_MAX_THICKNESS_RATIO: f64 = 0.75;
pub const FURIGANA_MAX_GAP_EM: f64 = 0.30;
pub const FURIGANA_MIN_CENTRE_OFFSET_EM: f64 = 0.35;
pub const FURIGANA_MIN_MAIN_OVERLAP: f64 = 0.5;
pub const FURIGANA_GENEROUS_MAX_THICKNESS_RATIO: f64 = 1.35;
pub const FURIGANA_MAX_GLYPH_PITCH_RATIO: f64 = 0.85;
pub const FURIGANA_LATTICE_MAX_PITCH_FRACTION: f64 = 0.70;
pub const LOW_CONFIDENCE: f64 = 0.5;
pub const FURIGANA_UNREADABLE_MAX_GLYPHS: usize = 2;
pub const FURIGANA_UNREADABLE_MAX_RATIO: f64 = 0.6;
pub const FURIGANA_TINY_MAX_RATIO: f64 = 0.45;
pub const FURIGANA_LATTICE_MAX_BODY_RATIO: f64 = 0.65;

pub const LONG_LINE_EM: f64 = 12.0;
pub const BODY_MIN_LONG_COLUMNS: usize = 3;
pub const BODY_MIN_COLUMNS: usize = 5;
pub const BODY_SIZE_RATIO: f64 = 1.35;
pub const BODY_EXTENT_SLACK_EM: f64 = 1.0;
pub const BAND_GUTTER_COVERAGE: f64 = 0.15;
pub const BODY_EDGE_CLUSTER_EM: f64 = 0.35;
pub const BODY_HANGING_REACH_EM: f64 = 1.5;
pub const BODY_GAP_FACTOR: f64 = 1.6;

pub const MARGIN_BAND_FRACTION: f64 = 0.15;
pub const MARGIN_CLEARANCE_EM: f64 = 0.25;
pub const MARGIN_MAX_LENGTH_FRACTION: f64 = 0.6;
pub const MARGIN_MIN_BODY_COVERAGE: f64 = 0.5;
pub const LONE_GLYPHS_MAX: usize = 2;
pub const LONE_GLYPHS_MIN_CONF: f64 = 0.9;

pub const MERGE_MAX_SIZE_RATIO: f64 = 2.5;
pub const MERGE_GAP_EM: f64 = 0.75;
pub const MERGE_GAP_MIXED_SIZE_EM: f64 = 0.50;
pub const MERGE_MIXED_SIZE_RATIO: f64 = 1.8;
pub const MERGE_ALIGNED_GAP_EM: f64 = 1.25;
pub const MERGE_ALIGNED_SIZE_RATIO: f64 = 1.35;
pub const MERGE_ALIGNED_START_EM: f64 = 0.3;
pub const MERGE_LOOSE_GAP_EM: f64 = 1.0;
pub const MERGE_LOOSE_START_EM: f64 = 0.5;
pub const MERGE_MIN_MAIN_OVERLAP: f64 = 0.5;
pub const MERGE_STAGGER_EM: f64 = 2.0;
pub const STITCH_MAX_GAP_EM: f64 = 0.6;
pub const STITCH_MIN_CROSS_OVERLAP: f64 = 0.7;
pub const STITCH_MAX_SIZE_RATIO: f64 = 1.6;
pub const BODY_STITCH_MAX_GAP_EM: f64 = 3.0;
pub const BODY_QUAD_MARGIN_EM: f64 = 0.05;

pub const PARAGRAPH_INDENT_MIN_EM: f64 = 0.6;
pub const BRACKET_INK_INSET_EM: f64 = 0.45;
pub const PARAGRAPH_SHORT_END_EM: f64 = 1.25;
pub const PARAGRAPH_SOFT_END_EM: f64 = 0.3;
pub const PARAGRAPH_DEEP_INSET_EM: f64 = 1.6;

pub const ROW_MIN_OVERLAP: f64 = 0.2;
pub const ROW_CHAIN_OVERLAP: f64 = 0.5;
pub const COLUMN_CLUSTER_EM: f64 = 0.5;
