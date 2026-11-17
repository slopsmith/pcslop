//! A plain-text log of register accesses. Runs of identical lines collapse
//! into one line and a count, so polling loops stay short.

use std::io::Write;

pub struct Trace {
    out: Box<dyn Write + Send>,
    last: String,
    repeats: u64,
}

impl Trace {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Trace { out, last: String::new(), repeats: 0 }
    }

    pub fn line(&mut self, text: String) {
        if text == self.last {
            self.repeats += 1;
            return;
        }
        self.flush_repeats();
        let _ = writeln!(self.out, "{text}");
        self.last = text;
    }

    fn flush_repeats(&mut self) {
        if self.repeats > 0 {
            let _ = writeln!(self.out, "  ... {} more times", self.repeats);
            self.repeats = 0;
        }
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.flush_repeats();
        let _ = self.out.flush();
    }
}
