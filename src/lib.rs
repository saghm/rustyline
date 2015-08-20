//!Readline for Rust
//!
//!This implementation is based on [Antirez's Linenoise](https://github.com/antirez/linenoise)
//!
//!# Example
//!
//!Usage
//!
//!```
//!let readline = rustyline::readline(">> ", &mut None);
//!match readline {
//!     Ok(line) => println!("Line: {:?}",line),
//!     Err(_)   => println!("No input"),
//! }
//!```
#![feature(drain)]
#![feature(io)]
#![feature(str_char)]
#![feature(unicode)]
extern crate libc;
extern crate nix;
extern crate unicode_width;

#[allow(non_camel_case_types)]
mod consts;
pub mod error;
pub mod history;

use std::fmt;
use std::io;
use std::str;
use std::io::{Write, Read};
use std::result;
use nix::errno::Errno;
use nix::sys::termios;

use consts::{KeyPress, char_to_key_press};
use history::History;

/// The error type for I/O and Linux Syscalls (Errno)
pub type Result<T> = result::Result<T, error::ReadlineError>;

// Represent the state during line editing.
struct State<'out, 'prompt> {
    out: &'out mut Write,
    prompt: &'prompt str, // Prompt to display
    prompt_width: usize, // Prompt Unicode width
    buf: String, // Edited line buffer
    pos: usize, // Current cursor position (byte position)
    cols: usize, // Number of columns in terminal
    history_index: usize, // The history index we are currently editing.
    history_end: String, // Current edited line before history browsing
    bytes: [u8; 4],
}

impl<'out, 'prompt> State<'out, 'prompt> {
    fn new(out: &'out mut Write, prompt: &'prompt str, capacity: usize, cols: usize, history_index: usize) -> State<'out, 'prompt> {
        State {
            out: out,
            prompt: prompt,
            prompt_width: unicode_width::UnicodeWidthStr::width(prompt),
            buf: String::with_capacity(capacity),
            pos: 0,
            cols: cols,
            history_index: history_index,
            history_end: String::new(),
            bytes: [0; 4],
        }
    }
}

impl<'out, 'prompt> fmt::Debug for State<'out, 'prompt> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("State")
            .field("prompt", &self.prompt)
            .field("prompt_width", &self.prompt_width)
            .field("buf", &self.buf)
            .field("buf length", &self.buf.len())
            .field("buf capacity", &self.buf.capacity())
            .field("pos", &self.pos)
            .field("cols", &self.cols)
            .field("history_index", &self.history_index)
            .field("history_end", &self.history_end)
            .finish()
    }
}

/// Maximum buffer size for the line read
static MAX_LINE: usize = 4096;

/// Unsupported Terminals that don't support RAW mode
static UNSUPPORTED_TERM: [&'static str; 3] = ["dumb","cons25","emacs"];

/// Check to see if STDIN is a TTY
fn is_a_tty() -> bool {
    let isatty = unsafe { libc::isatty(libc::STDIN_FILENO as i32) } != 0;
    isatty
}

/// Check to see if the current `TERM` is unsupported
fn is_unsupported_term() -> bool {
    use std::ascii::AsciiExt;
    match std::env::var("TERM") {
        Ok(term) => {
            let mut unsupported = false;
            for iter in &UNSUPPORTED_TERM {
                unsupported = (*iter).eq_ignore_ascii_case(&term)
            }
            unsupported
        }
        Err(_) => false
    }
}

fn from_errno(errno: Errno) -> error::ReadlineError {
    error::ReadlineError::from(nix::Error::from_errno(errno))
}

/// Enable raw mode for the TERM
fn enable_raw_mode() -> Result<termios::Termios> {
    use nix::sys::termios::{BRKINT, ICRNL, INPCK, ISTRIP, IXON, OPOST, CS8, ECHO, ICANON, IEXTEN, ISIG, VMIN, VTIME};
    if !is_a_tty() {
        Err(from_errno(Errno::ENOTTY))
    } else {
        let original_term = try!(termios::tcgetattr(libc::STDIN_FILENO));
        let mut raw = original_term;
        raw.c_iflag = raw.c_iflag   & !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
        raw.c_oflag = raw.c_oflag   & !(OPOST);
        raw.c_cflag = raw.c_cflag   | (CS8);
        raw.c_lflag = raw.c_lflag   & !(ECHO | ICANON | IEXTEN | ISIG);
        raw.c_cc[VMIN] = 1;
        raw.c_cc[VTIME] = 0;
        try!(termios::tcsetattr(libc::STDIN_FILENO, termios::TCSAFLUSH, &raw));
        Ok(original_term)
    }
}

/// Disable Raw mode for the term
fn disable_raw_mode(original_termios: termios::Termios) -> Result<()> {
    try!(termios::tcsetattr(libc::STDIN_FILENO,
                            termios::TCSAFLUSH,
                            &original_termios));
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
const TIOCGWINSZ: libc::c_ulong = 0x40087468;

#[cfg(any(target_os = "linux", target_os = "android"))]
const TIOCGWINSZ: libc::c_ulong = 0x5413;

/// Try to get the number of columns in the current terminal,
/// or assume 80 if it fails.
#[cfg(any(target_os = "linux",
          target_os = "android",
          target_os = "macos",
          target_os = "freebsd"))]
fn get_columns() -> usize {
    use std::mem::zeroed;
    use libc::c_ushort;
    use nix::sys::ioctl;

    unsafe {
        #[repr(C)]
        struct winsize {
            ws_row: c_ushort,
            ws_col: c_ushort,
            ws_xpixel: c_ushort,
            ws_ypixel: c_ushort
        }

        let mut size: winsize = zeroed();
        match ioctl::read_into(libc::STDOUT_FILENO, TIOCGWINSZ, &mut size) {
            Ok(_) => size.ws_col as usize, // TODO getCursorPosition
            Err(_) => 80,
        }
    }
}

fn write_and_flush(w: &mut Write, buf: &[u8]) -> Result<()> {
    try!(w.write_all(buf));
    try!(w.flush());
    Ok(())
}

/// Clear the screen. Used to handle ctrl+l
fn clear_screen(out: &mut Write) -> Result<()> {
    write_and_flush(out, b"\x1b[H\x1b[2J")
}

/// Beep, used for completion when there is nothing to complete or when all
/// the choices were already shown.
/*fn beep() -> Result<()> {
    write_and_flush(&mut io::stderr(), b"\x07")
}*/

// Control characters are treated as having zero width.
fn width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Rewrite the currently edited line accordingly to the buffer content,
/// cursor position, and number of columns of the terminal.
fn refresh_line(s: &mut State) -> Result<()> {
    use std::fmt::Write;
    use unicode_width::UnicodeWidthChar;

    let buf = &s.buf;
    let mut start = 0;
    let mut w1 = width(&buf[start..s.pos]);
    while s.prompt_width + w1 >= s.cols {
        let ch = buf.char_at(start);
        start += ch.len_utf8();
        w1 -= UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    let mut end = buf.len();
    let mut w2 = width(&buf[start..end]);
    while s.prompt_width + w2 > s.cols {
        let ch = buf.char_at_reverse(end);
        end -= ch.len_utf8();
        w2 -= UnicodeWidthChar::width(ch).unwrap_or(0);
    }

    let mut ab = String::new();
    // Cursor to left edge
    ab.push('\r');
    // Write the prompt and the current buffer content
    ab.push_str(s.prompt);
    ab.push_str(&s.buf[start..end]);
    // Erase to right
    ab.push_str("\x1b[0K");
    // Move cursor to original position.
    ab.write_fmt(format_args!("\r\x1b[{}C", w1 + s.prompt_width)).unwrap();
    write_and_flush(s.out, ab.as_bytes())
}

/// Insert the character `ch` at cursor current position.
fn edit_insert(s: &mut State, ch: char) -> Result<()> {
    if s.buf.len() < s.buf.capacity() {
        if s.buf.len() == s.pos {
            s.buf.push(ch);
            let size = ch.encode_utf8(&mut s.bytes).unwrap();
            s.pos += size;
            if s.prompt_width + width(&s.buf) < s.cols {
                // Avoid a full update of the line in the trivial case.
                write_and_flush(s.out, &mut s.bytes[0..size])
            } else {
                refresh_line(s)
            }
        } else {
            s.buf.insert(s.pos, ch);
            s.pos += ch.len_utf8();
            refresh_line(s)
        }
    } else {
        Ok(())
    }
}

/// Move cursor on the left.
fn edit_move_left(s: &mut State) -> Result<()> {
    if s.pos > 0 {
        let ch = s.buf.char_at_reverse(s.pos);
        s.pos -= ch.len_utf8();
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Move cursor on the right.
fn edit_move_right(s: &mut State) -> Result<()> {
    if s.pos != s.buf.len() {
        let ch = s.buf.char_at(s.pos);
        s.pos += ch.len_utf8();
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Move cursor to the start of the line.
fn edit_move_home(s: &mut State) -> Result<()> {
    if s.pos > 0 {
        s.pos = 0;
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Move cursor to the end of the line.
fn edit_move_end(s: &mut State) -> Result<()> {
    if s.pos != s.buf.len() {
        s.pos = s.buf.len();
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Delete the character at the right of the cursor without altering the cursor
/// position. Basically this is what happens with the "Delete" keyboard key.
fn edit_delete(s: &mut State) -> Result<()> {
    if s.buf.len() > 0 && s.pos < s.buf.len() {
        s.buf.remove(s.pos);
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Backspace implementation.
fn edit_backspace(s: &mut State) -> Result<()> {
    if s.pos > 0 && s.buf.len() > 0 {
        let ch = s.buf.char_at_reverse(s.pos);
        s.pos -= ch.len_utf8();
        s.buf.remove(s.pos);
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Kill the text from point to the end of the line.
fn edit_kill_line(s: &mut State) -> Result<()> {
    if s.buf.len() > 0 && s.pos < s.buf.len() {
        s.buf.drain(s.pos..);
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Kill backward from point to the beginning of the line.
fn edit_discard_line(s: &mut State) -> Result<()> {
    if s.pos > 0 && s.buf.len() > 0 {
        s.buf.drain(..s.pos);
        s.pos = 0;
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Exchange the char before cursor with the character at cursor.
fn edit_transpose_chars(s: &mut State) -> Result<()> {
    if s.pos > 0 && s.pos < s.buf.len() {
        let ch = s.buf.remove(s.pos);
        let size = ch.len_utf8();
        let och = s.buf.char_at_reverse(s.pos);
        let osize = och.len_utf8();
        s.buf.insert(s.pos - osize, ch);
        if s.pos != s.buf.len()-size {
            s.pos += size;
        } else {
            if size >= osize {
                s.pos += size - osize;
            } else {
                s.pos -= osize - size;
            }
        }
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Delete the previous word, maintaining the cursor at the start of the
/// current word.
fn edit_delete_prev_word(s: &mut State) -> Result<()> {
    if s.pos > 0 {
        let old_pos = s.pos;
        let mut ch = s.buf.char_at_reverse(s.pos);
        while s.pos > 0 && ch.is_whitespace() {
            s.pos -= ch.len_utf8();
            ch = s.buf.char_at_reverse(s.pos);
        }
        while s.pos > 0 && !ch.is_whitespace() {
            s.pos -= ch.len_utf8();
            ch = s.buf.char_at_reverse(s.pos);
        }
        s.buf.drain(s.pos..old_pos);
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Substitute the currently edited line with the next or previous history
/// entry.
fn edit_history_next(s: &mut State, history: &mut History, prev: bool) -> Result<()> {
    if history.len() > 0 {
        if s.history_index == history.len() {
            if prev {
                // Save the current edited line before to overwrite it
                s.history_end = s.buf.clone();
            } else {
                return Ok(());
            }
        } else if s.history_index == 0 && prev {
            return Ok(());
        }
        if prev {
            s.history_index -= 1;
        } else {
            s.history_index += 1;
        }
        if s.history_index < history.len() {
            s.buf = history.get(s.history_index).unwrap().clone();
        } else {
            s.buf = s.history_end.clone(); // TODO how to avoid cloning?
        }
        s.pos = s.buf.len();
        if s.buf.capacity() < MAX_LINE {
            let cap = s.buf.capacity();
            s.buf.reserve_exact(MAX_LINE - cap);
        }
        refresh_line(s)
    } else {
        Ok(())
    }
}

/// Handles reading and editting the readline buffer.
/// It will also handle special inputs in an appropriate fashion
/// (e.g., C-c will exit readline)
fn readline_edit(prompt: &str, history: &mut Option<History>) -> Result<String> {
    let mut stdout = io::stdout();
    try!(write_and_flush(&mut stdout, prompt.as_bytes()));

    let mut s = State::new(&mut stdout, prompt, MAX_LINE, get_columns(), history.as_mut().map_or(0, |h| h.len()));
    let stdin = io::stdin();
    let mut chars = stdin.lock().chars();
    loop {
        let ch = try!(chars.next().unwrap());
        match char_to_key_press(ch) {
            KeyPress::CTRL_A => try!(edit_move_home(&mut s)), // Move to the beginning of line.
            KeyPress::CTRL_B => try!(edit_move_left(&mut s)), // Move back a character.
            KeyPress::CTRL_C => {
                return Err(from_errno(Errno::EAGAIN))
            },
            KeyPress::CTRL_D => {
                if s.buf.len() > 0 { // Delete one character at point.
                    try!(edit_delete(&mut s))
                } else {
                    return Err(error::ReadlineError::Eof)
                }
            },
            KeyPress::CTRL_E => try!(edit_move_end(&mut s)), // Move to the end of line.
            KeyPress::CTRL_F => try!(edit_move_right(&mut s)), // Move forward a character.
            KeyPress::CTRL_H | KeyPress::BACKSPACE => try!(edit_backspace(&mut s)), // Delete one character backward.
            KeyPress::CTRL_K => try!(edit_kill_line(&mut s)), // Kill the text from point to the end of the line.
            KeyPress::CTRL_L => { // Clear the screen leaving the current line at the top of the screen.
                try!(clear_screen(s.out));
                try!(refresh_line(&mut s))
            },
            KeyPress::CTRL_N => { // Fetch the next command from the history list.
                if history.is_some() {
                    try!(edit_history_next(&mut s, history.as_mut().unwrap(), false))
                }
            },
            KeyPress::CTRL_P => { // Fetch the previous command from the history list.
                if history.is_some() {
                    try!(edit_history_next(&mut s, history.as_mut().unwrap(), true))
                }
            },
            KeyPress::CTRL_T => try!(edit_transpose_chars(&mut s)), // Exchange the char before cursor with the character at cursor.
            KeyPress::CTRL_U => try!(edit_discard_line(&mut s)), // Kill backward from point to the beginning of the line.
            KeyPress::CTRL_W => try!(edit_delete_prev_word(&mut s)), // Kill the word behind point, using white space as a word boundary
            KeyPress::ESC    => { // escape sequence
                // Read the next two bytes representing the escape sequence.
                let seq1 = try!(chars.next().unwrap());
                if seq1 == '[' { // ESC [ sequences.
                    let seq2 = try!(chars.next().unwrap());
                    if seq2.is_digit(10) { // Extended escape, read additional byte.
                        let seq3 = try!(chars.next().unwrap());
                        if seq3 == '~' {
                            match seq2 {
                                '3' => try!(edit_delete(&mut s)),
                                _ => (),
                            }
                        }
                    } else {
                        match seq2 {
                            'A' => { // Up
                                if history.is_some() {
                                    try!(edit_history_next(&mut s, history.as_mut().unwrap(), true))
                                }
                            },
                            'B' => { // Down
                                if history.is_some() {
                                    try!(edit_history_next(&mut s, history.as_mut().unwrap(), false))
                                }
                            },
                            'C' => { // Right
                                try!(edit_move_right(&mut s))
                            },
                            'D' => { // Left
                                try!(edit_move_left(&mut s))
                            },
                            'H' => { // Home
                                try!(edit_move_home(&mut s))
                            },
                            'F' => { // End
                                try!(edit_move_end(&mut s))
                            },
                            _ => ()
                        }
                    }
                } else if seq1 == 'O' { // ESC O sequences.
                    let seq2 = try!(chars.next().unwrap());
                    match seq2 {
                        'H' =>  try!(edit_move_home(&mut s)),
                        'F' => try!(edit_move_end(&mut s)),
                        _ => ()
                    }
                }
            },
            KeyPress::ENTER  => break, // Accept the line regardless of where the cursor is.
            _      => try!(edit_insert(&mut s, ch)), // Insert the character typed.
        }
    }
    Ok(s.buf)
}

/// Readline method that will enable RAW mode, call the ```readline_edit()```
/// method and disable raw mode
fn readline_raw(prompt: &str, history: &mut Option<History>) -> Result<String> {
    if is_a_tty() {
        let original_termios = try!(enable_raw_mode());
        let user_input = readline_edit(prompt, history);
        try!(disable_raw_mode(original_termios));
        println!("");
        user_input
    } else {
        readline_direct()
    }
}

fn readline_direct() -> Result<String> {
        let mut line = String::new();
        try!(io::stdin().read_line(&mut line));
        Ok(line)
}

/// This method will read a line from STDIN and will display a `prompt`
pub fn readline(prompt: &str, history: &mut Option<History>) -> Result<String> {
    if is_unsupported_term() {
        // Write prompt and flush it to stdout
        let mut stdout = io::stdout();
        try!(write_and_flush(&mut stdout, prompt.as_bytes()));

        readline_direct()
    } else {
        readline_raw(prompt, history)
    }
}

#[cfg(test)]
mod test {
    use std::io::Write;
    use history::History;
    use State;

    fn init_state<'out>(out: &'out mut Write, line: &str, pos: usize, cols: usize) -> State<'out, 'static> {
        State {
            out : out,
            prompt: "",
            prompt_width: 0,
            buf: String::from(line),
            pos: pos,
            cols: cols,
            history_index: 0,
            history_end: String::new(),
            bytes: [0; 4],
        }
    }

    #[test]
    fn insert() {
        let mut out = ::std::io::sink();
        let mut s = State::new(&mut out, "", 128, 80, 0);
        super::edit_insert(&mut s, 'α').unwrap();
        assert_eq!("α", s.buf);
        assert_eq!(2, s.pos);

        super::edit_insert(&mut s, 'ß').unwrap();
        assert_eq!("αß", s.buf);
        assert_eq!(4, s.pos);

        s.pos = 0;
        super::edit_insert(&mut s, 'γ').unwrap();
        assert_eq!("γαß", s.buf);
        assert_eq!(2, s.pos);
    }

    #[test]
    fn moves() {
        let mut out = ::std::io::sink();
        let mut s = init_state(&mut out, "αß", 4, 80);
        super::edit_move_left(&mut s).unwrap();
        assert_eq!("αß", s.buf);
        assert_eq!(2, s.pos);

        super::edit_move_right(&mut s).unwrap();
        assert_eq!("αß", s.buf);
        assert_eq!(4, s.pos);

        super::edit_move_home(&mut s).unwrap();
        assert_eq!("αß", s.buf);
        assert_eq!(0, s.pos);

        super::edit_move_end(&mut s).unwrap();
        assert_eq!("αß", s.buf);
        assert_eq!(4, s.pos);
    }

    #[test]
    fn delete() {
        let mut out = ::std::io::sink();
        let mut s = init_state(&mut out, "αß", 2, 80);
        super::edit_delete(&mut s).unwrap();
        assert_eq!("α", s.buf);
        assert_eq!(2, s.pos);

        super::edit_backspace(&mut s).unwrap();
        assert_eq!("", s.buf);
        assert_eq!(0, s.pos);
    }

    #[test]
    fn kill() {
        let mut out = ::std::io::sink();
        let mut s = init_state(&mut out, "αßγδε", 6, 80);
        super::edit_kill_line(&mut s).unwrap();
        assert_eq!("αßγ", s.buf);
        assert_eq!(6, s.pos);

        s.pos = 4;
        super::edit_discard_line(&mut s).unwrap();
        assert_eq!("γ", s.buf);
        assert_eq!(0, s.pos);
    }

    #[test]
    fn transpose() {
        let mut out = ::std::io::sink();
        let mut s = init_state(&mut out, "aßc", 1, 80);
        super::edit_transpose_chars(&mut s).unwrap();
        assert_eq!("ßac", s.buf);
        assert_eq!(3, s.pos);

        s.buf = String::from("aßc");
        s.pos = 3;
        super::edit_transpose_chars(&mut s).unwrap();
        assert_eq!("acß", s.buf);
        assert_eq!(2, s.pos);
    }

    #[test]
    fn delete_prev_word() {
        let mut out = ::std::io::sink();
        let mut s = init_state(&mut out, "a ß  c", 6, 80);
        super::edit_delete_prev_word(&mut s).unwrap();
        assert_eq!("a c", s.buf);
        assert_eq!(2, s.pos);
    }

    #[test]
    fn edit_history_next() {
        let mut out = ::std::io::sink();
        let line = "current edited line";
        let mut s = init_state(&mut out, line, 6, 80);
        let mut history = History::new();
        history.add("line0");
        history.add("line1");
        s.history_index = history.len();
        s.buf = String::from(line);

        for _ in 0..2 {
            super::edit_history_next(&mut s, &mut history, false).unwrap();
            assert_eq!(line, s.buf);
        }

        super::edit_history_next(&mut s, &mut history, true).unwrap();
        assert_eq!(line, s.history_end);
        assert_eq!(1, s.history_index);
        assert_eq!("line1", s.buf);

        for _ in 0..2 {
            super::edit_history_next(&mut s, &mut history, true).unwrap();
            assert_eq!(line, s.history_end);
            assert_eq!(0, s.history_index);
            assert_eq!("line0", s.buf);
        }

        super::edit_history_next(&mut s, &mut history, false).unwrap();
        assert_eq!(line, s.history_end);
        assert_eq!(1, s.history_index);
        assert_eq!("line1", s.buf);

        super::edit_history_next(&mut s, &mut history, false).unwrap();
        assert_eq!(line, s.history_end);
        assert_eq!(2, s.history_index);
        assert_eq!(line, s.buf);
    }
}
