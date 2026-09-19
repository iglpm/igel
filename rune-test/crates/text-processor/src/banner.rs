/// Generates an ASCII banner framed with decorative box characters.
pub fn render_banner(title: &str, subtitle: &str) -> String {
    let width = std::cmp::max(title.len(), subtitle.len()) + 8;
    let border = "═".repeat(width);

    let title_pad = width - 4 - title.len();
    let left_pad = title_pad / 2;
    let right_pad = title_pad - left_pad;

    let sub_pad = width - 4 - subtitle.len();
    let sub_left = sub_pad / 2;
    let sub_right = sub_pad - sub_left;

    format!(
        "╔{border}╗\n\
         ║  {}{}{}  ║\n\
         ║  {}{}{}  ║\n\
         ╚{border}╝",
        " ".repeat(left_pad), title, " ".repeat(right_pad),
        " ".repeat(sub_left), subtitle, " ".repeat(sub_right),
    )
}
