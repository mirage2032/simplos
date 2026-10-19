# Terminal

The screen is a terminal, and the keyboard types into it, as on Linux's
console: a program's standard input, output and error are the terminal,
`/dev/tty` too, and what it reads is what's typed. Between them is Linux's
line discipline (`tty/`): in canonical mode, the default, a line is typed
and edited first, with Backspace, ^U, ^W, and ^D for the end of file, and
handed over on Enter; in raw mode each key is ready as it's pressed. A
program reads and changes the settings, laid out as Linux's `termios`, with
`ioctl` (`TCGETS`, `TCSETS` and the rest), and asks the size with
`TIOCGWINSZ`.

The screen (`tty/terminal.rs`) keeps a grid of characters, each with its colours
and attributes, and a cursor; it draws only the rows that changed, and
scrolls the whole screen by moving it. It understands the escape sequences
the Linux console does, as its `linux` terminfo entry lists them, and
programs are given `TERM=linux`: moving the cursor, erasing, inserting and
deleting characters and lines, scrolling a region, the 16 colours and 256 and
any, bold, underlined and reversed, saving the cursor, hiding it, and saying
where it is, which comes back as if typed; and the alternate screen, which
full-screen programs ask for whatever the entry says. It looks like the
Linux console too: light grey on black, and bold text bright, as the
console draws it. The keys send what the console's do. In the shell, `run
colors` shows the colours.

`poll`, `ppoll`, `select` and `pselect6` wait for pipes and terminals, as
long as they're asked to, `ppoll` and `pselect6` with the signals they're
given blocked meanwhile, as busybox's shells wait in `read`; and
`O_NONBLOCK` (from `open`, `pipe2`, `fcntl` or `FIONBIO`) makes what would
wait fail with `EAGAIN` instead. In the shell, `run cat` copies what's typed
until ^D. For now:

- of the escape sequences, no line-drawing characters, tab stops but every
  eighth column, insert mode or application cursor keys, none of which the
  `linux` entry asks for; what isn't known is read and ignored;
- in raw mode, `VTIME` is ignored: a read waits for `VMIN` bytes however
  long they take;
- the font is ASCII: anything else shows as `?`.
