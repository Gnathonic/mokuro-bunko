//! `line_layout.py`: raw OCR lines in, mokuro blocks in reading order out.
//!
//! measure -> orientations -> furigana -> bodies -> roles -> merge ->
//! paragraphs/order -> reading order -> build blocks -> grow boxes over ruby
//! (spec §6). Deterministic: every tie is broken by input index.

mod block;
mod bodies;
pub mod consts;
mod furigana;
mod line;
mod merge;
mod order;

pub use block::{Block, build_block, grow_boxes_over_ruby, page_box};
pub use bodies::{Body, Role, classify_roles, find_bodies, text_start};
pub use furigana::{Ruby, column_lattice, filter_furigana, ruby_of};
pub use line::{
    Line, Point, Quad, Spans, box_distance, canonical_quad, decide_orientations, dominant_vertical,
    is_ambiguous, measure, measure_lines, overlap, pair_theta, quad_frame,
};
pub use merge::{column_pieces, is_column_piece, merge_lines, should_merge};
pub use order::{Kind, block_theta, cluster_columns, group_box, order_blocks, order_lines, split_paragraphs};

use crate::records::RawPage;

/// Result of [`layout_page`].
///
/// `blocks` are in reading order; `groups[k]` holds, for block `k`, the
/// input line indices in the block's line order; `kinds[k]` its role.
/// `dropped` lists lines with no text or a degenerate quad.
#[derive(Debug, Clone, PartialEq)]
pub struct PageLayout {
    pub blocks: Vec<Block>,
    pub groups: Vec<Vec<usize>>,
    pub kinds: Vec<Kind>,
    pub ruby: Vec<Ruby>,
    pub dropped: Vec<usize>,
    pub bodies: Vec<Body>,
}

impl PageLayout {
    /// Every line index that belongs to some text body (`∪ body.members`).
    pub fn body_members(&self) -> std::collections::BTreeSet<usize> {
        self.bodies.iter().flat_map(|b| b.members.iter().copied()).collect()
    }

    /// Line indices removed as ruby.
    pub fn ruby_lines(&self) -> std::collections::BTreeSet<usize> {
        self.ruby.iter().map(|r| r.line).collect()
    }
}

/// Raw page lines -> mokuro blocks in reading order, furigana removed.
///
/// `page` must already be rounded the way `ppocr.page_to_json` rounds
/// ([`RawPage::rounded`]); the layout sees exactly those numbers.
pub fn layout_page(page: &RawPage) -> PageLayout {
    let width = page.width as f64;
    let height = page.height as f64;
    let (mut lines, dropped) = measure_lines(&page.lines);
    decide_orientations(&mut lines);
    let (kept_pos, ruby) = filter_furigana(&lines);
    let kept: Vec<Line> = kept_pos.iter().map(|&p| lines[p].clone()).collect();
    let bodies = find_bodies(&kept);
    let roles = classify_roles(&kept, &bodies, height, width);

    let mut groups: Vec<Vec<&Line>> = Vec::new();
    let mut kinds: Vec<Kind> = Vec::new();
    let mut caps: Vec<f64> = Vec::new();
    for group_pos in merge_lines(&kept, &bodies, &roles) {
        let group: Vec<&Line> = group_pos.iter().map(|&p| &kept[p]).collect();
        let role = group
            .iter()
            .map(|l| roles.get(&l.index).copied().unwrap_or(Role::Text))
            .find(|r| *r != Role::Noise)
            .unwrap_or(Role::Noise);
        let indices: std::collections::BTreeSet<usize> = group.iter().map(|l| l.index).collect();
        let mut body: Option<&Body> = None;
        let mut best = 0usize;
        for b in &bodies {
            let n = indices.intersection(&b.members).count();
            if body.is_none() || n > best {
                body = Some(b);
                best = n;
            }
        }
        match body {
            Some(b) if role == Role::Text && 2 * best >= indices.len() => {
                for paragraph in split_paragraphs(&group, b) {
                    groups.push(paragraph);
                    kinds.push(Kind::Body);
                    caps.push(b.gap / 2.0);
                }
            }
            _ => {
                groups.push(order_lines(&group));
                kinds.push(match role {
                    Role::Text => Kind::Text,
                    Role::Header => Kind::Header,
                    Role::Footer => Kind::Footer,
                    Role::Noise => Kind::Noise,
                });
                caps.push(0.0);
            }
        }
    }

    let order = order_blocks(&groups, &kinds, &bodies);
    let mut blocks: Vec<Block> = order.iter().map(|&i| build_block(&groups[i], width, height, caps[i])).collect();
    let ordered_groups: Vec<Vec<usize>> = order.iter().map(|&i| groups[i].iter().map(|l| l.index).collect()).collect();
    let ordered_kinds: Vec<Kind> = order.iter().map(|&i| kinds[i]).collect();
    grow_boxes_over_ruby(&mut blocks, &ordered_groups, &ordered_kinds, &ruby, width, height);
    PageLayout { blocks, groups: ordered_groups, kinds: ordered_kinds, ruby, dropped, bodies }
}
