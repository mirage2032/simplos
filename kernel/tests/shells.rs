//! Shells: busybox's and zsh, and the commands they run, uutils' coreutils
//! and busybox's others, as `scripts/userland.sh` builds them and
//! `tools/boot-test` puts them on this kernel's disk, as it does for a
//! kernel whose name starts with `shells`. Each runs on a terminal of its
//! own: as a command, its output kept; or as people use them, typed at.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::fs::{self, Handle, ROOT};
use simplos::process::{self, Process};
use simplos::tty::Tty;
use simplos::{init, thread};
use spin::Mutex;

entry_point!(main, config = &simplos::BOOTLOADER_CONFIG);

fn main(boot_info: &'static mut BootInfo) -> ! {
    simplos::pre_init(boot_info);
    fs::init();
    test_main();
    simplos::hlt_loop()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    simplos::test_panic_handler(info)
}

/// What the first program gets, as `init` gives it.
const ENVIRONMENT: [&str; 2] = simplos::init::ENVIRONMENT;

/// Run `argv` on a terminal no one types on: how it ended, and what it
/// wrote, newlines as they are.
fn run(argv: &[&str]) -> (i32, String) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let tty = Tty::capture(Box::new(move |bytes| sink.lock().extend_from_slice(bytes)));
    let files = vec![Some(Arc::new(Handle::tty(Arc::new(tty)))); 3];
    let program = process::spawn_program(argv[0], argv, &ENVIRONMENT, files).expect("it runs");
    let status = program.wait();
    let output = output.lock().clone();
    (status, String::from_utf8_lossy(&output).into_owned())
}

/// What's done at a terminal, in turn. To interrupt a job, wait for it to
/// say it started, on a line of its own, not in the command echoed: until
/// it does, its process can be the shell's still, which handles ^C itself,
/// between giving it the terminal and running it.
enum Step<'a> {
    /// Wait for the screen to show this, past what was waited for before.
    Shown(&'a str),
    /// Type this.
    Typed(&'a str),
    /// Wait for the shell to give the terminal to another group, as it does
    /// to a job it runs in the foreground.
    Away,
}
use Step::*;

/// Run `argv` on a terminal of its own, as someone would, with `steps`;
/// then wait for it to end: how it did, and all it wrote.
fn session(argv: &[&str], steps: &[Step]) -> (i32, String) {
    let start = |tty: &Arc<Tty>| {
        let files = vec![Some(Arc::new(Handle::tty(tty.clone()))); 3];
        process::spawn_program(argv[0], argv, &ENVIRONMENT, files).expect("it runs")
    };
    terminal(start, steps)
}

/// `session` for the program `start` starts on a terminal, 24 rows by 80.
fn terminal(start: impl FnOnce(&Arc<Tty>) -> Arc<Process>, steps: &[Step]) -> (i32, String) {
    let screen = Arc::new(Mutex::new(Vec::new()));
    let sink = screen.clone();
    let tty = Arc::new(Tty::new(
        Box::new(move |bytes: &[u8]| sink.lock().extend_from_slice(bytes)),
        24,
        80,
    ));
    let program = start(&tty);
    let mut seen = 0;
    // Wait for `done`, or fail saying what for, and what's on the screen.
    let until = |what: &str, done: &mut dyn FnMut() -> bool| {
        for _ in 0..1000 {
            if done() {
                return;
            }
            thread::sleep(10);
        }
        panic!(
            "waited 10 s for {what:?}; the screen:\n{}",
            String::from_utf8_lossy(&screen.lock())
        );
    };
    for step in steps {
        match *step {
            Shown(text) => until(text, &mut || match find(&screen.lock()[seen..], text) {
                Some(at) => {
                    seen += at + text.len();
                    true
                }
                None => false,
            }),
            Typed(text) => tty.input(text.as_bytes()),
            Away => until("another group", &mut || tty.foreground() != program.pgid()),
        }
    }
    let status = program.wait();
    let screen = screen.lock().clone();
    (status, String::from_utf8_lossy(&screen).into_owned())
}

/// Where `text` first is in `bytes`.
fn find(bytes: &[u8], text: &str) -> Option<usize> {
    bytes.windows(text.len()).position(|window| window == text.as_bytes())
}

fn ok(text: &str) -> (i32, String) {
    (0, String::from(text))
}

#[test_case]
fn commands_run_through_their_links() {
    // uutils' (ls, cat, wc, sort...) and busybox's (sed, awk, xargs...), as
    // their links name them. ls through a pipe, where it lists one to a
    // line, without colours.
    let script = "echo $((6 * 7)); ls /bin/ls /hello.txt | cat; cat /hello.txt | wc -c; \
                  uname -s; printf '%s\\n' c b a | sort | tr a-z A-Z | head -2; \
                  seq 3 | xargs echo; expr 6 \\* 7; basename /a/b.txt .txt; \
                  echo hello | sed s/l/L/g; \
                  printf 'a 1\\nb 2\\n' | awk '{ s += $2 } END { print s }'; \
                  id -u; whoami; echo $0";
    assert_eq!(
        run(&["/bin/sh", "-c", script]),
        ok("42\n/bin/ls\n/hello.txt\n16\nLinux\nA\nB\n1 2 3\n42\nb\nheLLo\n3\n0\nroot\n/bin/sh\n")
    );
}

#[test_case]
fn commands_work_with_files() {
    // Made, linked, found, copied, moved, changed, edited in place and
    // appended to; then all of it removed.
    let script = "cd /tmp && mkdir -p a/b && echo x > a/b/f && ln -s f a/b/l && readlink a/b/l; \
                  cat a/b/l; find a | sort; cp /hello.txt a/c && mv a/c a/d && cat a/d; \
                  chmod 750 a/d && stat -c %a a/d; touch -d @1000000000 a/d && stat -c %Y a/d; \
                  sed -i s/hello/HELLO/ a/d && cat a/d; echo more >> a/d; tail -n 1 a/d; \
                  wc -l < a/d; rm -r a && ls -A /tmp | wc -l";
    assert_eq!(
        run(&["/bin/sh", "-c", script]),
        ok("f\nx\na\na/b\na/b/f\na/b/l\nhello from ext4\n750\n1000000000\nHELLO from ext4\n\
            more\n2\n0\n")
    );
}

#[test_case]
fn uutils_commands_use_threads_timers_and_pipes() {
    // What the kernel gives uutils that busybox never asked for: sort,
    // with a thread of its own, of numbers out of order; du, whose sizes a
    // thread prints; dd, whose progress a thread counts; timeout, waiting by
    // a timer for a command that ends in time and one that doesn't; cat,
    // through splice; cp -a, which finds no extended attributes to copy; ln
    // -s by a relative path, which rustix passes AT_FDCWD for; df, from
    // /proc/self/mountinfo; and which they are.
    let script = "seq 1000 | shuf | sort -n | tail -n 1; du -s /etc | cut -f 2; \
                  dd if=/hello.txt of=/tmp/dd bs=4 status=none && cat /tmp/dd; \
                  timeout 5 true; echo $?; timeout 1 sleep 5; echo $?; \
                  cd /tmp && cp -a /etc e && ln -s e/passwd p && cat p | wc -l; \
                  rm -r e p dd; df / | tail -n 1 | cut -c 1-9; \
                  ls --version | head -n 1 | cut -d ' ' -f 1-3";
    assert_eq!(
        run(&["/bin/sh", "-c", script]),
        ok("1000\n/etc\nhello from ext4\n0\n124\n1\n/dev/root\nls (uutils coreutils)\n")
    );
}

#[test_case]
fn busybox_sh_runs_a_script() {
    // Functions, a case, loops, parameters cut down, a subshell, read from a
    // here-document, a trap at the end, and a job in the background waited
    // for; run by sh, then by itself, through its #! line.
    let script = r#"cat > /tmp/script <<'END'
#!/bin/sh
twice() { echo "$1$1"; }
for word in "$@"; do
    case $word in
    a*) twice "$word" ;;
    *) echo "${word%.txt}" ;;
    esac
done
i=0
while [ $i -lt 3 ]; do i=$((i + 1)); done
(cd /usr && echo "in $(pwd), $i")
read first rest <<EOF
one two three
EOF
echo "$first|$rest"
trap 'echo bye' EXIT
sleep 0.1 &
wait $! && echo waited
END
chmod +x /tmp/script; sh /tmp/script ab c.txt; /tmp/script ax; rm /tmp/script"#;
    let once = "abab\nc\nin /usr, 3\none|two three\nwaited\nbye\n";
    let again = "axax\nin /usr, 3\none|two three\nwaited\nbye\n";
    assert_eq!(run(&["/bin/sh", "-c", script]), ok(&(String::from(once) + again)));
}

#[test_case]
fn zsh_runs_commands() {
    // Its expansions, arrays and hashes; a here-document and a here-string,
    // which it writes to files it removes while they're open; globbing;
    // arithmetic, and the time from a module; who it runs as; and a script
    // through its #! line.
    let script = r#"print -l ${(U)${:-a b}}; a=(1 2 3); print ${#a} ${a[-1]} ${(j:,:)a}
typeset -A h; h=(k v); print $h[k]
x=$(print sub); cat <<EOF
here $x
EOF
cat <<< "string $x"; print -l /bin/(|z)sh(N) /usr/share/zsh/*/functions/compinit(N:t)
print $(( 2 ** 10 )); zmodload zsh/datetime; print $(( EPOCHSECONDS > 1700000000 ))
print ${(%):-%n} $ZSH_VERSION; cd /usr/share && print $PWD
print '#!/bin/zsh -f\nprint ran $0 $1' > /tmp/z && chmod +x /tmp/z && /tmp/z arg; rm /tmp/z
ls -A /tmp | wc -l"#;
    assert_eq!(
        run(&["/bin/zsh", "-f", "-c", script]),
        ok("A B\n3 3 1,2,3\nv\nhere sub\nstring sub\n/bin/sh\n/bin/zsh\ncompinit\n1024\n1\n\
            root 5.9.2\n/usr/share\nran /tmp/z arg\n0\n")
    );
}

#[test_case]
fn zsh_is_typed_at() {
    // A command; one edited before it's run; a job in the background, killed
    // by its number; one in the foreground, interrupted with ^C; and out.
    let (status, screen) = session(
        &["/bin/zsh", "-f", "-i"],
        &[
            Shown("simplos# "),
            Typed("echo typed in\r"),
            Shown("\r\ntyped in\r\n"),
            Shown("simplos# "),
            Typed("echo abc\x7fd\r"),
            Shown("\r\nabd\r\n"),
            Shown("simplos# "),
            Typed("sleep 20 &\r"),
            Shown("[1] "),
            Shown("simplos# "),
            Typed("jobs\r"),
            Shown("running"),
            Shown("simplos# "),
            Typed("kill %1\r"),
            Shown("terminated"),
            Shown("simplos# "),
            Typed("sh -c 'echo started; sleep 20'\r"),
            Away,
            Shown("started\r\n"),
            Typed("\x03"),
            Shown("simplos# "),
            Typed("echo status $?\r"),
            Shown("status 130"),
            Shown("simplos# "),
            Typed("exit\r"),
        ],
    );
    assert_eq!(status, 0, "{screen}");
}

#[test_case]
fn busybox_sh_is_typed_at() {
    // The same of busybox's sh, but for the jobs.
    let (status, screen) = session(
        &["/bin/sh", "-i"],
        &[
            Shown("~ # "),
            Typed("echo typed in\r"),
            Shown("\r\ntyped in\r\n"),
            Shown("~ # "),
            Typed("echo abc\x7fd\r"),
            Shown("\r\nabd\r\n"),
            Shown("~ # "),
            Typed("sh -c 'echo started; sleep 20'\r"),
            Away,
            Shown("started\r\n"),
            Typed("\x03"),
            Shown("~ # "),
            Typed("echo status $?\r"),
            Shown("status 130"),
            Shown("~ # "),
            Typed("exit\r"),
        ],
    );
    assert_eq!(status, 0, "{screen}");
}

#[test_case]
fn busybox_sh_s_read_ends_for_a_signal() {
    // ^C, and a signal it traps, end `read` waiting for a line, as on
    // Linux, though busybox blocks every signal but while it waits, in
    // ppoll.
    let (status, screen) = session(
        &["/bin/sh", "-i"],
        &[
            Shown("~ # "),
            Typed("echo waiting; read x; echo read $?\r"),
            Shown("\r\nwaiting\r\n"),
            Typed("\x03"),
            Shown("~ # "),
            Typed("echo status $?\r"),
            Shown("status 130"),
            Shown("~ # "),
            Typed("trap 'echo trapped' USR1; (sleep 0.2; kill -USR1 $$) & read x; echo read $?\r"),
            Shown("\r\ntrapped\r\nread 1\r\n"),
            Shown("~ # "),
            Typed("exit\r"),
        ],
    );
    assert_eq!(status, 0, "{screen}");
}

#[test_case]
fn vim_edits_a_file() {
    // Typed into, a line read in from a shell it runs, and written, which
    // stays written once it's quit.
    let (status, screen) = session(
        &["/usr/bin/vim", "/tmp/vim.txt"],
        &[
            Shown("\"/tmp/vim.txt\" [New]"),
            Typed("ihello from vim"),
            Shown("hello from vim"),
            Typed("\x1b"),
            Typed(":r !echo from a shell\r"),
            Shown("from a shell"),
            Typed(":wq\r"),
        ],
    );
    assert_eq!(status, 0, "{screen}");
    let root = ROOT.lock();
    let fs = root.as_ref().unwrap();
    let file = fs.open("tmp/vim.txt").unwrap();
    let mut written = vec![0; file.size().unwrap() as usize];
    file.read_at(0, &mut written).unwrap();
    assert_eq!(String::from_utf8_lossy(&written), "hello from vim\nfrom a shell\n");
    drop(file);
    fs.remove_file("tmp/vim.txt").unwrap();
}

#[test_case]
fn the_first_program_is_root_s_zsh() {
    // /sbin/init, a script, logs root in to zsh, which reads /etc/zshrc:
    // its prompt; root's home; a command and a file name completed; one
    // run again from the history, which is kept in a file; Home and End;
    // and out, which leaves the kernel's own shell to take over.
    let mut path = String::new();
    let start = |tty: &Arc<Tty>| {
        let (started, first) = init::start(tty, &init::PROGRAMS, &[]).expect("a first program");
        path = started;
        first
    };
    let (status, screen) = terminal(
        start,
        &[
            Shown("root@simplos"),
            Shown("# "),
            Typed("print $HOME $SHELL $USER $PWD\r"),
            Shown("\r\n/root /bin/zsh root /root\r\n"),
            Shown("# "),
            Typed("whoam\t"),
            Shown("whoami"),
            Typed("\r"),
            Shown("\r\nroot\r\n"),
            Shown("# "),
            Typed("cat /etc/pass\t"),
            Shown("passwd"),
            Typed("\r"),
            Shown("\r\nroot:x:0:0:root:/root:/bin/zsh\r\n"),
            Shown("# "),
            Typed("\x1b[A\r"),
            Shown("\r\nroot:x:0:0:root:/root:/bin/zsh\r\n"),
            Shown("# "),
            Typed("o at the end\x1b[1~ech\x1b[4~ too\r"),
            Shown("\r\nat the end too\r\n"),
            Shown("# "),
            Typed("exit\r"),
        ],
    );
    assert_eq!((path.as_str(), status), ("/sbin/init", 0), "{screen}");
    let root = ROOT.lock();
    let fs = root.as_ref().unwrap();
    let file = fs.open("root/.zsh_history").unwrap();
    let mut history = vec![0; file.size().unwrap() as usize];
    file.read_at(0, &mut history).unwrap();
    let history = String::from_utf8_lossy(&history);
    assert!(history.contains("cat /etc/passwd\n"), "{history}");
}

#[test_case]
fn the_system_shows_itself_in_proc() {
    // free and top, from /proc/meminfo and /proc/stat; ps, the processes
    // there are and what they're doing, the shell, a job of its and ps
    // itself; df, from /proc/mounts and statfs, and stat's of it; mount;
    // and what a process reads of itself. ps once the job is sleep, not
    // still the shell it was forked from.
    let script = "free | awk 'NR == 2 { print $1, ($2 > 0), ($4 > 0) }'; \
                  top -b -n 1 | awk 'NR == 1 { print $1, ($2 + 0 > 0), ($4 + 0 > 0) } \
                                     NR == 2 || NR == 3 { print $1 }'; \
                  sleep 10 & until read job < /proc/$!/comm && [ $job = sleep ]; do :; done; \
                  listed=$(ps -o stat,comm); kill $!; echo \"$listed\" | tail -n +2; \
                  df / | awk 'NR == 2 { print $1, $2, ($3 > 0), $6 }'; stat -f -c '%T %b' /; \
                  mount; cat /proc/self/cmdline | tr '\\0' ' '; echo; \
                  readlink /proc/self | grep -c '^[0-9][0-9]*$'";
    assert_eq!(
        run(&["/bin/sh", "-c", script]),
        ok("Mem: 1 1\nMem: 1 1\nCPU:\nLoad\nS    sh\nS    sleep\nR    ps\n/dev/root 131072 1 /\n\
            ext2/ext3 32768\n/dev/root on / type ext4 (rw,relatime)\n\
            devtmpfs on /dev type devtmpfs (rw)\nproc on /proc type proc (rw)\n\
            cat /proc/self/cmdline \n1\n")
    );
}

#[test_case]
fn descriptors_can_be_opened_by_name() {
    // /dev/stderr, /dev/fd/N and /proc/self/fd/N: what they're open on.
    // And zsh's <(...), a pipe it names /proc/self/fd/N, beside =(...), a
    // file it removes after.
    let script = "echo to stderr > /dev/stderr; exec 3< /hello.txt; cat /dev/fd/3; \
                  [ -e /proc/self/fd/0 ] && echo there";
    assert_eq!(run(&["/bin/sh", "-c", script]), ok("to stderr\nhello from ext4\nthere\n"));
    let script = "cat <(print from a pipe) =(print from a file)";
    assert_eq!(run(&["/bin/zsh", "-f", "-c", script]), ok("from a pipe\nfrom a file\n"));
}
