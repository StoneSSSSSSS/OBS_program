use super::{clock, theme, Window, Workbench};
use cv_core::editor::Caption;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui};
use std::{cmp::Reverse, collections::BinaryHeap};

#[derive(Default)]
pub(super) struct CaptionRows {
    revision: Option<u64>,
    rows: Vec<Vec<usize>>,
}

impl CaptionRows {
    fn update(&mut self, revision: u64, cues: &[Caption]) {
        if self.revision == Some(revision) { return; }
        self.revision = Some(revision);
        self.rows.clear();
        let mut indices: Vec<_> = (0..cues.len()).collect();
        indices.sort_unstable_by_key(|&i| (cues[i].range.start, cues[i].id));
        let mut ends = BinaryHeap::<Reverse<(u64, usize)>>::new();
        for i in indices {
            let cue = &cues[i];
            let row = match ends.peek().copied() {
                Some(Reverse((end, row))) if end <= cue.range.start => { ends.pop(); row }
                _ => { self.rows.push(Vec::new()); self.rows.len() - 1 }
            };
            self.rows[row].push(i);
            ends.push(Reverse((cue.range.end, row)));
        }
    }
}

impl Workbench {
    pub(super) fn caption_lane(&mut self, ui: &mut Ui, axis: Rect, window: Window, accent: Color32) {
        let cues = &self.history.project.captions;
        if cues.is_empty() { return; }
        self.timeline.caption_rows.update(self.revision, cues);
        let rows = &self.timeline.caption_rows.rows;
        let mut action = None;
        let height = 23.0;
        egui::ScrollArea::vertical().id_salt("editor-caption-tracks")
            .max_height((height + ui.spacing().item_spacing.y) * 2.0)
            .auto_shrink([false, true]).show_rows(ui, height, rows.len(), |ui, visible| {
                for row in &rows[visible] {
                    let (track, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
                    let content = Rect::from_min_max(Pos2::new(axis.left(), track.top()), Pos2::new(axis.right().min(track.right()), track.bottom()));
                    if !content.is_positive() { continue; }
                    let painter = ui.painter_at(track);
                    painter.rect_filled(track, 3.0, theme::SIDEBAR);
                    painter.text(track.left_center() + egui::vec2(6.0, 0.0), Align2::LEFT_CENTER,
                        "Captions", FontId::proportional(12.0), theme::TEXT_DIM);
                    let first = row.partition_point(|&i| cues[i].range.end <= window.start);
                    for &i in &row[first..] {
                        let cue = &cues[i];
                        if cue.range.start >= window.start.saturating_add(window.span) { break; }
                        let left = window.x(axis, cue.range.start);
                        let right = window.x(axis, cue.range.end);
                        let clip = Rect::from_min_max(Pos2::new(left, track.top() + 1.0),
                            Pos2::new(right.max(left + 1.0), track.bottom() - 1.0)).intersect(content);
                        if !clip.is_positive() { continue; }
                        let response = ui.interact(clip, ui.make_persistent_id(("caption-span", cue.id)), Sense::click());
                        let first_line = cue.text.lines().next().unwrap_or("");
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true,
                            format!("Caption {first_line}, {} to {}", clock(cue.range.start), clock(cue.range.end))));
                        let border = if response.hovered() || response.has_focus() { theme::TEXT } else { theme::BORDER_STRONG };
                        painter.rect(clip, 2.0, theme::SURFACE2, Stroke::new(1.0, border), StrokeKind::Inside);
                        if clip.width() > 18.0 {
                            ui.painter_at(clip.shrink2(egui::vec2(4.0, 0.0))).text(clip.left_center() + egui::vec2(4.0, 0.0),
                                Align2::LEFT_CENTER, first_line, FontId::proportional(11.0), theme::TEXT);
                        }
                        let edit = response.double_clicked();
                        if edit || response.clicked() {
                            let end = !edit && response.interact_pointer_pos().is_some_and(|p| {
                                right <= content.right() && right - p.x <= (clip.width() * 0.25).min(9.0)
                            });
                            action = Some((i, if end { cue.range.end } else { cue.range.start }, edit));
                        }
                        response.on_hover_text(format!("{} – {}\n{}\nClick to seek · Right edge for end · Double-click to edit",
                            clock(cue.range.start), clock(cue.range.end), cue.text));
                    }
                    let x = window.x(axis, self.playhead);
                    if content.x_range().contains(x) {
                        painter.line_segment([Pos2::new(x, track.top()), Pos2::new(x, track.bottom())], Stroke::new(1.6, accent));
                    }
                }
            });
        if let Some((i, at, edit)) = action {
            if edit { self.captions.open_cue(&cues[i]); }
            self.pause();
            self.seek(at, ui.ctx());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::editor::Span;

    #[test]
    fn touching_and_overlapping_cues_stay_individually_selectable() {
        let cues = [(5, 12), (0, 5), (1, 8), (8, 9)].into_iter().enumerate()
            .map(|(i, (start, end))| Caption { id: i as u64 + 1, range: Span { start, end }, text: String::new() })
            .collect::<Vec<_>>();
        let mut cache = CaptionRows::default();
        cache.update(1, &cues);
        assert_eq!(cache.rows, [vec![1, 0], vec![2, 3]]);
        cache.update(2, &cues[..1]);
        assert_eq!(cache.rows, [vec![0]]);
    }
}
