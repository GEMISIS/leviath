//! The same canvas, drawn once into a buffer and returned as text, for
//! `lev validate --graph`. Plain glyphs, no colour: the CLI prints plain
//! text everywhere else too.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use super::content::NodeStyle;
use super::model::StageGraph;
use super::view::FlowView;

/// Draw `graph` at most `width` columns wide. Escape edges are included:
/// there is no key to reveal them on paper.
pub(crate) fn render_to_text(graph: &StageGraph, width: u16) -> String {
    let mut view = FlowView::new(Arc::new(graph.clone()), NodeStyle::Full, true);
    view.toggle_escape();
    view.fit();
    let (world_w, world_h) = view.world_extent();
    // Room for the edge lanes above and below the nodes and a cell of margin
    // each side; the fit-view keeps zoom at 1.0 when everything fits and
    // shrinks the picture otherwise.
    let lanes = view.max_stem().ceil() as u16;
    let width = width.max(20);
    let cols = (world_w.ceil() as u16).saturating_add(4).min(width);
    let rows = (world_h.ceil() as u16).saturating_add(2 * lanes + 4);
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);
    view.render_into(area, &mut buf);
    let mut lines: Vec<String> = Vec::with_capacity(rows as usize);
    for y in 0..rows {
        let line: String = (0..cols)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
            .collect();
        lines.push(line.trim_end().to_string());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

impl FlowView {
    /// Draw straight into a buffer, for the text render.
    pub(crate) fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        self.flow_mut().render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::manifest::parse_manifest;

    #[test]
    fn every_bundled_agent_renders_every_stage_without_trailing_spaces() {
        for agent in crate::bundled::BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(path, _)| *path == "agent.leviath")
                .map(|(_, content)| *content)
                .expect("a bundled agent has a manifest");
            let graph = StageGraph::from_blueprint(&parse_manifest(manifest).expect("parses"));
            let text = render_to_text(&graph, 400);
            for id in graph.ids() {
                let shown = id.trim_start_matches("ext:");
                assert!(text.contains(shown), "{}: {shown} in\n{text}", agent.name);
            }
            assert!(
                text.lines().all(|l| l == l.trim_end()),
                "{}: trailing spaces",
                agent.name
            );
            assert!(!text.ends_with('\n'));
        }
    }

    #[test]
    fn a_narrow_width_shrinks_the_picture_and_the_floor_is_twenty_columns() {
        let graph = StageGraph::from_blueprint(
            &parse_manifest(
                r#"
[agent]
name = "wide"
[stages.first_stage_name]
[stages.second_stage_name]
[stages.third_stage_name]
"#,
            )
            .unwrap(),
        );
        let wide = render_to_text(&graph, 200);
        let narrow = render_to_text(&graph, 5);
        assert!(wide.lines().map(|l| l.chars().count()).max().unwrap() > 60);
        assert!(narrow.lines().all(|l| l.chars().count() <= 20), "{narrow}");
        assert!(!narrow.is_empty());
        // A manifest with no stages gets the parser's default one.
        let bare = StageGraph::from_blueprint(&parse_manifest("[agent]\nname = \"e\"\n").unwrap());
        assert!(render_to_text(&bare, 40).contains("main"));
    }
}
