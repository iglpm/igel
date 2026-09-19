/// ASCII table formatter for CLI output.
pub struct AsciiTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl AsciiTable {
    pub fn new(headers: Vec<&str>) -> Self {
        Self {
            headers: headers.into_iter().map(|s| s.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn add_row(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    pub fn render(&self) -> String {
        let num_cols = self.headers.len();
        let mut col_widths = vec![0; num_cols];

        for (i, header) in self.headers.iter().enumerate() {
            col_widths[i] = std::cmp::max(col_widths[i], header.len());
        }

        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < num_cols {
                    col_widths[i] = std::cmp::max(col_widths[i], cell.len());
                }
            }
        }

        let mut output = String::new();
        
        // Header top border
        output.push('+');
        for w in &col_widths {
            output.push_str(&"-".repeat(w + 2));
            output.push('+');
        }
        output.push('\n');

        // Header titles
        output.push('|');
        for (i, header) in self.headers.iter().enumerate() {
            output.push_str(&format!(" {:<width$} |", header, width = col_widths[i]));
        }
        output.push('\n');

        // Separator
        output.push('+');
        for w in &col_widths {
            output.push_str(&"=".repeat(w + 2));
            output.push('+');
        }
        output.push('\n');

        // Data rows
        for row in &self.rows {
            output.push('|');
            for i in 0..num_cols {
                let val = row.get(i).map(|s| s.as_str()).unwrap_or("");
                output.push_str(&format!(" {:<width$} |", val, width = col_widths[i]));
            }
            output.push('\n');
        }

        // Bottom border
        output.push('+');
        for w in &col_widths {
            output.push_str(&"-".repeat(w + 2));
            output.push('+');
        }
        output.push('\n');

        output
    }
}
