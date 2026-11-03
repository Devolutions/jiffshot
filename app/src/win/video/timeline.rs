//! Non-destructive video cuts, expressed as retained ranges of the original file.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub start: i64,
    pub end: i64,
}

impl Span {
    pub fn duration(self) -> i64 {
        self.end - self.start
    }
}

#[derive(Debug)]
pub(super) struct Timeline {
    spans: Vec<Span>,
    undo: Vec<Vec<Span>>,
    redo: Vec<Vec<Span>>,
    saved: Vec<Span>,
}

impl Timeline {
    pub fn new(duration: i64) -> Result<Self, String> {
        if duration <= 0 {
            return Err("The video has no playable duration.".into());
        }
        let spans = vec![Span {
            start: 0,
            end: duration,
        }];
        Ok(Self {
            saved: spans.clone(),
            spans,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    pub fn duration(&self) -> i64 {
        self.spans.iter().map(|span| span.duration()).sum()
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn is_dirty(&self) -> bool {
        self.spans != self.saved
    }

    pub fn mark_saved(&mut self, spans: Vec<Span>) {
        self.saved = spans;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Maps the edited playhead to the original, choosing the next clip at a join.
    pub fn source_time(&self, edited: i64) -> i64 {
        let mut offset = edited.clamp(0, self.duration());
        for span in &self.spans {
            if offset < span.duration() {
                return span.start + offset;
            }
            offset -= span.duration();
        }
        self.spans
            .last()
            .expect("a timeline retains at least one clip")
            .end
    }

    /// Source ranges for a selection on the edited timeline.
    pub fn pieces(&self, from: i64, to: i64) -> Vec<Span> {
        let mut offset = 0;
        let mut pieces = Vec::new();
        for span in &self.spans {
            let start = from.max(offset);
            let end = to.min(offset + span.duration());
            if end > start {
                pieces.push(Span {
                    start: span.start + start - offset,
                    end: span.start + end - offset,
                });
            }
            offset += span.duration();
        }
        pieces
    }

    pub fn joins(&self) -> Vec<f64> {
        let mut offset = 0;
        self.spans
            .iter()
            .take(self.spans.len().saturating_sub(1))
            .map(|span| {
                offset += span.duration();
                super::seconds(offset)
            })
            .collect()
    }

    pub fn cut(&mut self, from: i64, to: i64, minimum: i64) -> Result<(), String> {
        let duration = self.duration();
        if from < 0 || to > duration || to <= from {
            return Err("Select a section of the timeline to cut.".into());
        }
        if duration - (to - from) < minimum.max(1) {
            return Err("Keep at least one frame in the video.".into());
        }
        let mut kept = self.pieces(0, from);
        kept.extend(self.pieces(to, duration));
        self.undo.push(std::mem::replace(&mut self.spans, kept));
        self.redo.clear();
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let Some(spans) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.spans, spans));
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(spans) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.spans, spans));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_cuts_ripple_in_edited_time_and_preview_the_remaining_source() {
        let mut timeline = Timeline::new(100).unwrap();
        timeline.cut(20, 40, 1).unwrap();
        timeline.cut(30, 50, 1).unwrap();
        assert_eq!(timeline.duration(), 60);
        assert_eq!(
            timeline.spans(),
            &[
                Span { start: 0, end: 20 },
                Span { start: 40, end: 50 },
                Span {
                    start: 70,
                    end: 100
                },
            ]
        );
        assert_eq!(timeline.source_time(19), 19);
        assert_eq!(timeline.source_time(20), 40);
        assert_eq!(timeline.source_time(30), 70);
        assert_eq!(timeline.source_time(60), 100);
        assert_eq!(
            timeline.pieces(15, 35),
            vec![
                Span { start: 15, end: 20 },
                Span { start: 40, end: 50 },
                Span { start: 70, end: 75 },
            ]
        );
    }

    #[test]
    fn cutting_across_a_join_and_at_the_edges_removes_only_selected_content() {
        let mut timeline = Timeline::new(100).unwrap();
        timeline.cut(20, 40, 1).unwrap();
        timeline.cut(10, 30, 1).unwrap();
        assert_eq!(
            timeline.spans(),
            &[
                Span { start: 0, end: 10 },
                Span {
                    start: 50,
                    end: 100
                },
            ]
        );
        timeline.cut(0, 10, 1).unwrap();
        timeline.cut(40, 50, 1).unwrap();
        assert_eq!(timeline.spans(), &[Span { start: 50, end: 90 }]);
    }

    #[test]
    fn undo_redo_and_saved_snapshots_track_real_edits() {
        let mut timeline = Timeline::new(100).unwrap();
        timeline.cut(20, 40, 1).unwrap();
        let saving = timeline.spans().to_vec();
        timeline.cut(10, 20, 1).unwrap();
        timeline.mark_saved(saving.clone());
        assert!(
            timeline.is_dirty(),
            "edits during a save must remain unsaved"
        );
        assert!(timeline.undo());
        assert_eq!(timeline.spans(), saving);
        assert!(!timeline.is_dirty());
        assert!(timeline.redo());
        assert!(timeline.is_dirty());
        assert!(timeline.undo());
        timeline.cut(0, 5, 1).unwrap();
        assert!(
            !timeline.can_redo(),
            "a new cut must discard the redo branch"
        );
        assert!(timeline.undo());
        assert!(timeline.undo());
        assert_eq!(timeline.spans(), &[Span { start: 0, end: 100 }]);
        assert!(!timeline.can_undo());
    }

    #[test]
    fn invalid_or_empty_cuts_do_not_change_the_video_or_history() {
        let mut timeline = Timeline::new(100).unwrap();
        for (start, end) in [(-1, 10), (0, 101), (20, 20), (40, 20), (0, 100), (0, 99)] {
            assert!(timeline.cut(start, end, 2).is_err(), "{start}..{end}");
            assert_eq!(timeline.duration(), 100, "{start}..{end}");
            assert!(!timeline.can_undo(), "{start}..{end}");
        }
        timeline.cut(0, 98, 2).unwrap();
        assert_eq!(
            timeline.duration(),
            2,
            "exactly one minimum clip is allowed"
        );
        assert!(Timeline::new(0).is_err());
    }
}
