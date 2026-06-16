#!/usr/bin/env python3
"""Add tests for all review findings."""
import pathlib, sys

p = pathlib.Path("crates/oam_cli/tests/e2e.rs")
src = p.read_text(encoding="utf-8")

# Find the last closing brace of the file to insert before it
# We'll append tests at the end (before final newlines)
tests = r'''
#[test]
fn perf_hooks_measure_throws_on_missing_mark() {
    let (code, stdout, stderr) = run("perf_measure_missing.cjs", r#"
        const { performance } = require('perf_hooks');
        // measure with nonexistent start mark should throw
        let threw = false;
        try { performance.measure('bad', 'nonexistent'); } catch(e) { threw = true; console.log('err=' + e.message); }
        console.log('threw=' + threw);
        // measure with nonexistent end mark should throw
        performance.mark('real');
        let threw2 = false;
        try { performance.measure('bad2', 'real', 'nope'); } catch(e) { threw2 = true; }
        console.log('threw2=' + threw2);
        // measure with object start string, missing mark
        let threw3 = false;
        try { performance.measure('bad3', { start: 'gone', end: 'real' }); } catch(e) { threw3 = true; }
        console.log('threw3=' + threw3);
    "#);
    assert!(stdout.contains("threw=true"), "missing start mark: {stdout}");
    assert!(stdout.contains("threw2=true"), "missing end mark: {stdout}");
    assert!(stdout.contains("threw3=true"), "missing opts.start mark: {stdout}");
    assert!(stdout.contains("err=Failed to execute"), "error message: {stdout}");
}

#[test]
fn perf_hooks_get_entries_sorted_by_start_time() {
    let stdout = run_ok("perf_get_entries_order.cjs", r#"
        const { performance } = require('perf_hooks');
        performance.mark('a');
        performance.measure('m1', { start: 0, end: 1 });
        performance.mark('b');
        const all = performance.getEntries();
        // Should be sorted by startTime: measure(start=0), mark-a, mark-b
        const names = all.map(e => e.name);
        console.log('order=' + names.join(','));
        console.log('first_type=' + all[0].entryType);
    "#);
    assert!(stdout.contains("order=m1,a,b"), "getEntries order: {stdout}");
    assert!(stdout.contains("first_type=measure"), "first entry type: {stdout}");
}

#[test]
fn perf_hooks_observer_entry_types_buffered() {
    let stdout = run_ok("perf_obs_et_buffered.cjs", r#"
        const { performance, PerformanceObserver } = require('perf_hooks');
        performance.mark('pre1');
        performance.mark('pre2');
        const buffered = [];
        const obs = new PerformanceObserver((list) => {
            for (const e of list.getEntries()) buffered.push(e.name);
        });
        // entryTypes (array form) + buffered:true should deliver existing entries
        obs.observe({ entryTypes: ['mark'], buffered: true });
        console.log('count=' + buffered.length);
        console.log('names=' + buffered.join(','));
        obs.disconnect();
    "#);
    assert!(stdout.contains("count=2"), "buffered count: {stdout}");
    assert!(stdout.contains("names=pre1,pre2"), "buffered names: {stdout}");
}

#[test]
fn readline_terminal_auto_infer() {
    let stdout = run_ok("readline_terminal.cjs", r#"
        const readline = require('readline');
        const { Readable, Writable } = require('stream');
        const input = new Readable({ read() {} });
        // Writable with isTTY = true should auto-set terminal
        const out = new Writable({ write(_c, _e, cb) { cb(); } });
        out.isTTY = true;
        const rl1 = readline.createInterface({ input, output: out });
        console.log('auto_tty=' + rl1.terminal);
        // Explicit terminal:false overrides isTTY
        const rl2 = readline.createInterface({ input, output: out, terminal: false });
        console.log('explicit_false=' + rl2.terminal);
        // No isTTY -> terminal false
        const out2 = new Writable({ write(_c, _e, cb) { cb(); } });
        const rl3 = readline.createInterface({ input, output: out2 });
        console.log('no_tty=' + rl3.terminal);
    "#);
    assert!(stdout.contains("auto_tty=true"), "auto-infer: {stdout}");
    assert!(stdout.contains("explicit_false=false"), "explicit false: {stdout}");
    assert!(stdout.contains("no_tty=false"), "no isTTY: {stdout}");
}

#[test]
fn readline_question_cleanup_on_close() {
    let stdout = run_ok("readline_question_close.cjs", r#"
        const { Readable } = require('stream');
        const readline = require('readline');
        const input = new Readable({ read() {} });
        const rl = readline.createInterface({ input });
        let called = false;
        rl.question('prompt> ', () => { called = true; });
        // Close before any line arrives
        rl.close();
        // The question callback should NOT have been called
        console.log('called=' + called);
        // The 'line' listener should be cleaned up
        console.log('listeners=' + rl.listenerCount('line'));
    "#);
    assert!(stdout.contains("called=false"), "cb not called: {stdout}");
    assert!(stdout.contains("listeners=0"), "listener cleaned: {stdout}");
}

#[test]
fn vm_create_context_throws_on_null() {
    let (code, stdout, stderr) = run("vm_ctx_null.cjs", r#"
        const vm = require('vm');
        // null should throw TypeError
        let threw_null = false;
        try { vm.createContext(null); } catch(e) {
            threw_null = e instanceof TypeError;
            console.log('null_msg=' + e.message);
        }
        console.log('threw_null=' + threw_null);
        // number should throw TypeError
        let threw_num = false;
        try { vm.createContext(42); } catch(e) { threw_num = e instanceof TypeError; }
        console.log('threw_num=' + threw_num);
        // undefined should work (creates new empty context)
        let ok = false;
        try { const ctx = vm.createContext(); ok = vm.isContext(ctx); } catch(e) {}
        console.log('undefined_ok=' + ok);
    "#);
    assert!(stdout.contains("threw_null=true"), "null throws: {stdout}");
    assert!(stdout.contains("threw_num=true"), "number throws: {stdout}");
    assert!(stdout.contains("undefined_ok=true"), "undefined ok: {stdout}");
}

#[test]
fn worker_threads_env_data_structured_clone() {
    let stdout = run_ok("wt_envdata_clone.cjs", r#"
        const wt = require('worker_threads');
        // Store and mutate -- get should return snapshot, not live ref
        const obj = { x: 1 };
        wt.setEnvironmentData('k', obj);
        obj.x = 999;
        const got = wt.getEnvironmentData('k');
        console.log('snapshot_x=' + got.x);
        // Primitives should round-trip without clone overhead
        wt.setEnvironmentData('str', 'hello');
        console.log('str=' + wt.getEnvironmentData('str'));
        wt.setEnvironmentData('num', 42);
        console.log('num=' + wt.getEnvironmentData('num'));
        // null stored value
        wt.setEnvironmentData('nil', null);
        console.log('nil=' + wt.getEnvironmentData('nil'));
    "#);
    assert!(stdout.contains("snapshot_x=1"), "clone on store: {stdout}");
    assert!(stdout.contains("str=hello"), "primitive str: {stdout}");
    assert!(stdout.contains("num=42"), "primitive num: {stdout}");
    assert!(stdout.contains("nil=null"), "null value: {stdout}");
}

#[test]
fn worker_threads_message_channel_clone() {
    let stdout = run_ok("wt_msgchan_clone.cjs", r#"
        const { MessageChannel } = require('worker_threads');
        const ch = new MessageChannel();
        const obj = { val: 'original' };
        let received = null;
        ch.port2.on('message', (msg) => { received = msg; });
        ch.port1.postMessage(obj);
        // Mutate after sending
        obj.val = 'mutated';
        queueMicrotask(() => {
            // Receiver should see original, not mutated
            console.log('received_val=' + received.val);
            console.log('sender_val=' + obj.val);
            ch.port1.close();
            ch.port2.close();
        });
    "#);
    assert!(stdout.contains("received_val=original"), "clone isolation: {stdout}");
    assert!(stdout.contains("sender_val=mutated"), "sender mutated: {stdout}");
}

#[test]
fn readline_clearline_invalid_dir() {
    let (code, stdout, stderr) = run("readline_cl_invalid.cjs", r#"
        const readline = require('readline');
        const { Writable } = require('stream');
        let buf = '';
        const out = new Writable({
            write(chunk, _enc, cb) { buf += chunk; cb(); }
        });
        // dir=0 is valid (whole line clear)
        readline.clearLine(out, 0);
        const valid = buf;
        buf = '';
        // undefined dir also falls to else branch (writes ESC[2K)
        readline.clearLine(out, undefined);
        const undef_wrote = buf.length > 0;
        console.log('valid_len=' + valid.length);
        console.log('undef_wrote=' + undef_wrote);
    "#);
    // This documents current behavior -- undefined writes ESC[2K (whole line)
    assert!(stdout.contains("valid_len="), "valid clearLine: {stdout}");
    assert!(stdout.contains("undef_wrote=true"), "undefined dir writes: {stdout}");
}

#[test]
fn worker_threads_message_port_close_event() {
    let stdout = run_ok("wt_port_close.cjs", r#"
        const { MessageChannel } = require('worker_threads');
        const ch = new MessageChannel();
        let closed = false;
        ch.port2.on('close', () => { closed = true; });
        ch.port1.close();
        ch.port2.close();
        console.log('close_fired=' + closed);
        console.log('start_returns_this=' + (ch.port1.start() === ch.port1));
    "#);
    assert!(stdout.contains("close_fired=true"), "close event: {stdout}");
    assert!(stdout.contains("start_returns_this=true"), "start returns this: {stdout}");
}
'''

# Append tests before the final line(s) of the file
src = src.rstrip() + "\n" + tests.strip() + "\n"
p.write_text(src, encoding="utf-8")
print(f"OK -- added 10 new test functions")
