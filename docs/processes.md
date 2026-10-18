# Processes, signals and time

## Processes

A program can start others as on Linux: `fork`, `vfork` and `clone` (as C
libraries use it for them) copy it, `execve` replaces it with an ELF program
from the disk, closing the descriptors marked close-on-exec, and `wait4`
waits for its copies and tells how each ended; `getpid` and `getppid` say
who's who. `execve` runs a script through what its first line names
(`#!/bin/sh -e` and the like), as Linux does, with the line's one argument
and the script's path; that can be a script too, five deep. A program
that names an interpreter (`PT_INTERP`), as a dynamically linked one names
its loader, starts in that, as on Linux: `execve` loads it too, where
`mmap` starts handing memory out, and tells it where it and the program
are (`AT_BASE`, `AT_PHDR`, `AT_ENTRY`), for it to map the shared libraries
the program needs and start it. So Rust's `std::process::Command` works: its `spawn`, `status`,
`output` and `exec`. `cargo run`'s
disk has the user program as `/bin/probe` (a `disk.img` made before this
doesn't: delete it), so in the shell, `run spawn /bin/probe hello` has it
run itself again.

A program can have threads, as on Linux (`process/`): `clone` with
`CLONE_THREAD`, as musl's `pthread_create` and so Rust's `std::thread` ask
for it, starts one that shares all of the program, its memory, descriptors,
working directory and signal handlers, with an id (`gettid`), `fs`, SSE
registers, signal mask and alternate stack of its own. Its threads wait on
each other with `futex` (`process/futex.rs`): waits, wakes and requeues, timed or
not, with bitsets, which musl's and Rust's mutexes, condition variables and
`pthread_join` are made of; as a thread ends, `clone`'s
`CLONE_CHILD_CLEARTID` or `set_tid_address` has 0 written to where it said
and a futex there woken. `exit` ends one thread, and the program once it's
the last; `exit_group`, a fault or a fatal signal ends them all. A thread
that calls `execve` takes the program over, its other threads ended, and a
thread that forks makes a copy with just itself. A signal for the program
goes to a thread that doesn't block it, `tkill` and `tgkill` to the one
named, and SIGSTOP stops them all. `membarrier` has nothing to wait for on
one CPU. The scheduler gives each thread its turns, as it does each program.

A program can ask what it runs on: `uname` says Linux, the interface it's
talking to, with simplos as the host and in the version; `sched_getaffinity`
gives it the one CPU; `sysinfo` how long it's been up, how much memory there
is and is free, and how many processes; it runs as root (`getuid` and the
like say 0), in no other groups (`getgroups`); `getpriority` and
`setpriority` keep how nice it is; and `getrlimit`, `setrlimit` and
`prlimit64` keep its limits, which its copies inherit. `readv` and
`writev` move several buffers in one call, as musl's `printf` and the like
do. `mremap` makes what `mmap` mapped longer or shorter, or moves it,
taking its pages along rather than copying them, as musl's `realloc` asks
of big blocks. `mprotect` with `PROT_NONE` puts memory out of the program's
reach and keeps what's in it for when it's given back, and gives the
program memory `mmap` reserved with `PROT_NONE`, as musl lays out a
thread's stack below its guard page. `getrandom`, as Rust's `std` asks for
its hash maps' keys, and `/dev/random` and `/dev/urandom` give random bytes
(`random.rs`). For now:

- every id is 0, and can't be set to another, as there are no other users;
  what `setgroups` sets is forgotten;
- limits and niceness are kept and reported, not enforced: a program has the
  1024 descriptors and 64 MiB of stack there are, whatever it sets, its
  stack mapped a page at a time as it's used, and the same turns at the CPU
  as any other;

- `clone` without `CLONE_THREAD` copies the program even when asked to share
  its memory (`CLONE_VM`), which is all `posix_spawn` needs, and
  `CLONE_VFORK` doesn't hold the parent back; a thread shares its
  program's descriptors and working directory, as every libc's do
  (`CLONE_FILES` and `CLONE_FS` go with `CLONE_THREAD`, and only with it);
- a futex is its program's alone, as `FUTEX_PRIVATE_FLAG`'s are; there are
  no priority-inheriting ones, nor `FUTEX_WAKE_OP`, nor robust lists, which
  musl keeps itself; and a timed wait that a signal ends starts its time
  again;
- there's no `/proc/PID/task`, and a thread's CPU time is its program's;
- a program that ended keeps its memory until it's waited for, and those
  never waited for go with their parent rather than to an init;
- `execve` takes 128 KiB of arguments and environment, their strings and
  pointers together, which it holds in the kernel's heap as it starts the
  program, where Linux takes a quarter of the stack's limit: more is
  `E2BIG`;
- an interpreter must be position-independent, as loaders are, rather than
  at addresses of its own;
- random bytes come from RDRAND when the CPU has it, which QEMU's default
  CPU hasn't, and otherwise from the timestamp counter, stirred, which is
  guessable;
- `mprotect` gives the program memory nothing reserved too, where Linux
  says `ENOMEM`; and won't make a file's page mapped `MAP_SHARED` writable
  (`EACCES`), where Linux writes what's written there to the file, which
  nothing here would;
- `mremap` grows a mapping where it is only if it's the last `mmap`
  handed out, where Linux grows into any room after it, and moves it
  otherwise; it takes no part that's `PROT_NONE`, and has no
  `MREMAP_DONTUNMAP`.

## Signals

Signals are Linux's (`process/signal.rs`): a program sends them with `kill`, to a
process or a process group, and with `tkill` and `tgkill`, to a thread; its terminal sends ^C's
SIGINT, ^\'s SIGQUIT and ^Z's SIGTSTP to the group in its foreground; and
the kernel sends SIGCHLD as a child ends, stops or continues, and SIGPIPE
as a pipe no one reads is written. Each does what the program asked with
`rt_sigaction`, or else its default: ending the program, stopping it until
SIGCONT, or nothing. One that's blocked (`rt_sigprocmask`) waits, pending
(`rt_sigpending`), until it isn't; `rt_sigsuspend` and `pause` wait for one,
and `rt_sigtimedwait`, which `sigwait` comes to, waits for one it blocks and
takes it rather than have its handler run, with its `siginfo_t`, for as long
as it's told: as uutils' `timeout` waits for its child's SIGCHLD or its
timer's SIGALRM.

A handler runs on the program's stack, or the one `sigaltstack` gave it,
with the frame Linux puts there: what the program was doing, its registers
and its SSE state, all put back by `rt_sigreturn`. A signal is acted on as
the program goes back to ring 3, as a system call returns or as the timer
interrupts it, so one that never calls into the kernel still gets it; and a
call that was waiting (a read, `wait4`, a sleep, `poll`) ends early, with
`EINTR`, or starts again if the handler asked for `SA_RESTART`. `wait4`
tells of children that stopped or continued if asked (`WUNTRACED`,
`WCONTINUED`), and a child's SIGCHLD can be ignored, which frees it as it
ends.

Programs are in process groups, in sessions (`setpgid`, `setsid`,
`getpgid`, `getsid`): a program the kernel starts leads one, with the
terminal it's on and its own group in the terminal's foreground, which
`TIOCSPGRP` changes, as a shell does for its jobs. As a session's leader
ends, its foreground is sent SIGHUP. In the shell, `run sleep 100000` is
ended by ^C. For now:

- a fault ends its program with its signal even if it has a handler for it;
- a signal whose default dumps core just ends the program, as there's
  nowhere to dump it;
- real-time signals aren't queued: one of each is kept at a time;
- a sleep that a stop, or a signal with no handler, interrupts starts its
  whole time again, where Linux's sleeps for what's left;
- a session's leader giving its terminal up (`TIOCNOTTY`) doesn't hang up
  the terminal's foreground;
- ^Z, SIGTTIN and SIGTTOU stop no one in a group no one outside it could
  continue, as POSIX says, which a program the kernel starts is in; a job a
  shell starts can be stopped;
- a program in the background isn't stopped for reading or writing its
  terminal, and one whose parent ended isn't given to an init.

## Time

The time of day comes from the CMOS real-time clock, read once at boot, and
the HPET carries it on from there (`time.rs`), or the timer's ticks, where
there's none, or its counter is 32 bits and would wrap in five minutes. Programs read it, and the
time since boot, with `clock_gettime`, `gettimeofday` and `time`, to the
nanosecond; `clock_nanosleep` sleeps for a while or until a time. Each timer
tick is counted to the program it interrupts, as its own time or the
kernel's for it, which `getrusage`, `times` and `wait4` report, its waited-for
children's too. `setitimer` and `alarm` set a program's alarm, which the
alarm thread sends as SIGALRM when it's due, and every so often after if
asked; and `timer_create` makes as many timers as a program likes, by the
time of day or since boot, each sending the signal it's set to, to the
program or one of its threads (musl's `SIGEV_THREAD`), or nothing, with
`SI_TIMER` and its value; `timer_settime` sets one for a time or after a
while, and every so often after, `timer_gettime` says when it comes and
`timer_delete` takes it away. A program's timers go as it `execve`s, and
its copies don't have them. For now:

- the clock is taken to be UTC, as QEMU keeps it, and can't be set;
- CPU time is counted to the tick, 10 ms;
- an alarm can come up to a tick late, though never early, and only the
  real-time one is there: none that counts CPU time (`ITIMER_VIRTUAL`,
  `ITIMER_PROF`); nor timers by the CPU-time clocks, and 256 at most;
- a timer that comes again before its signal was taken sends it again,
  where Linux counts the overrun: `timer_getoverrun` always says 0; and its
  value is the low 32 bits of what it was given.
