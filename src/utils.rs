const MAX_MESSAGE_SIZE: usize = 30_000;

pub enum CommandResponse {
    Text(String),
    Yaml(String),
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        footer: Option<String>,
    },
    Terminal(String),
}

impl CommandResponse {
    pub fn render_chunks(self) -> Vec<(String, String)> {
        match self {
            CommandResponse::Text(text) => Self::chunk_text(&text, None),
            CommandResponse::Yaml(yaml) => Self::chunk_text(&yaml, Some("yaml")),
            CommandResponse::Terminal(term) => Self::chunk_text(&term, Some("bash")),
            CommandResponse::Table {
                headers,
                rows,
                footer,
            } => Self::chunk_table(&headers, &rows, footer.as_deref()),
        }
    }

    fn chunk_text(text: &str, lang: Option<&str>) -> Vec<(String, String)> {
        let mut chunks = Vec::new();
        let mut current_chunk = String::new();

        for line in text.lines() {
            if current_chunk.len() + line.len() > MAX_MESSAGE_SIZE {
                if !current_chunk.is_empty() {
                    chunks.push(Self::format_text_chunk(&current_chunk, lang));
                    current_chunk.clear();
                }

                if line.len() > MAX_MESSAGE_SIZE {
                    let mut current = line;
                    while current.len() > MAX_MESSAGE_SIZE {
                        let mut split_at = MAX_MESSAGE_SIZE;
                        while !current.is_char_boundary(split_at) {
                            split_at -= 1;
                        }
                        chunks.push(Self::format_text_chunk(&current[..split_at], lang));
                        current = &current[split_at..];
                    }
                    if !current.is_empty() {
                        current_chunk.push_str(current);
                        current_chunk.push('\n');
                    }
                } else {
                    current_chunk.push_str(line);
                    current_chunk.push('\n');
                }
            } else {
                current_chunk.push_str(line);
                current_chunk.push('\n');
            }
        }

        if !current_chunk.is_empty() {
            chunks.push(Self::format_text_chunk(&current_chunk, lang));
        }

        chunks
    }

    fn format_text_chunk(text: &str, lang: Option<&str>) -> (String, String) {
        let plain = text.trim_end().to_string();

        let html = if let Some(l) = lang {
            let encoded = html_escape::encode_text(&plain);
            format!("<pre><code class=\"language-{l}\">{}</code></pre>", encoded)
        } else {
            let plain_for_md = plain.replace('\n', "  \n");

            let mut options = pulldown_cmark::Options::empty();
            options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
            options.insert(pulldown_cmark::Options::ENABLE_TABLES);

            let parser = pulldown_cmark::Parser::new_ext(&plain_for_md, options);
            let mut html_output = String::new();
            pulldown_cmark::html::push_html(&mut html_output, parser);

            html_output
        };

        (plain, html)
    }

    fn chunk_table(
        headers: &[String],
        rows: &[Vec<String>],
        footer: Option<&str>,
    ) -> Vec<(String, String)> {
        if headers.is_empty() && rows.is_empty() {
            return vec![Self::format_text_chunk("Empty table", None)];
        }

        let mut col_widths = vec![0; headers.len()];
        for (i, header) in headers.iter().enumerate() {
            col_widths[i] = header.len();
        }

        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                if i < col_widths.len() && cell.len() > col_widths[i] {
                    col_widths[i] = cell.len();
                }
            }
        }

        let mut text = String::new();

        for (i, header) in headers.iter().enumerate() {
            let width = col_widths[i];
            text.push_str(&format!("{header:<width$}"));
            if i < headers.len() - 1 {
                text.push_str(" | ");
            }
        }
        text.push('\n');

        for (i, &width) in col_widths.iter().enumerate() {
            text.push_str(&"-".repeat(width));
            if i < headers.len() - 1 {
                text.push_str("-|-");
            }
        }
        text.push('\n');

        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                if i < col_widths.len() {
                    let width = col_widths[i];
                    text.push_str(&format!("{cell:<width$}"));
                    if i < col_widths.len() - 1 {
                        text.push_str(" | ");
                    }
                }
            }
            text.push('\n');
        }

        if let Some(f) = footer {
            text.push('\n');
            text.push_str(f);
        }

        Self::chunk_text(&text, None)
    }
}
