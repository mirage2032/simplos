//! Shells: busybox's and zsh, as `scripts/userland.sh` builds them and
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
fn busybox_runs_its_applets_through_their_links() {
    // ls through a pipe, where it lists one to a line, without colours.
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
fn busybox_works_with_files() {
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
