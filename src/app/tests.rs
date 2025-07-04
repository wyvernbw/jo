#![cfg(test)]

use std::sync::Arc;

use sysinfo::Pid;

use crate::app::state::{Mode, SearchState, SearchTypeTransition, SortMode, State, Transition};

use std::sync::Once;

static INIT: Once = Once::new();

pub fn initialize() {
    INIT.call_once(|| {
        tracing_subscriber::fmt().init();
    });
}

#[test]
fn start_search_sets_mode_and_resets_old() {
    initialize();
    let st = State::default();
    assert!(matches!(st.mode, Mode::Normal));
    let st2 = Transition::StartSearch.transition(st);
    if let Mode::Search(ss) = st2.mode {
        assert_eq!(&*ss.term, "");
        assert_eq!(ss.idx, 0);
        assert!(ss.hooked);
    } else {
        panic!("Expected Search mode");
    }
    assert!(st2.old_search_state.is_none());
}

#[test]
fn typing_search_builds_term_and_hooks() {
    initialize();
    let st = Transition::StartSearch.transition(State::default());
    // type 'f'
    let st = Transition::SearchType(SearchTypeTransition::Type('f')).transition(st);
    if let Mode::Search(ss) = &st.mode {
        assert_eq!(&*ss.term, "f");
        assert!(ss.hooked, "should be hooked after typing");
    } else {
        panic!("Expected Search mode after typing");
    }
    // type 'o'
    let st = Transition::SearchType(SearchTypeTransition::Type('o')).transition(st);
    if let Mode::Search(ss) = &st.mode {
        assert_eq!(&*ss.term, "fo");
    } else {
        panic!("Expected Search mode after typing");
    }
}

#[test]
fn next_and_prev_search_result_adjust_idx_and_hook() {
    initialize();
    // start with an old_search_state to advance from
    let base_ss = SearchState {
        term: Arc::from("x"),
        idx: 0,
        hooked: false,
    };
    let mut st = State::default();
    st.old_search_state = Some(base_ss.clone());
    // next
    let st2 = Transition::NextSearchResult.transition(st);
    let ss2 = st2
        .old_search_state
        .as_ref()
        .expect("should have old_search_state");
    assert_eq!(ss2.idx, 1);
    assert!(ss2.hooked);
    // prev
    let st3 = Transition::PrevSearchResult.transition(st2);
    let ss3 = st3.old_search_state.expect("should have old_search_state");
    assert_eq!(ss3.idx, 0); // back to zero
    assert!(ss3.hooked);
}

#[test]
fn complete_search_returns_to_normal_and_keeps_old_search_state() {
    initialize();
    let st = Transition::StartSearch.transition(State::default());
    // type something
    let st = Transition::SearchType(SearchTypeTransition::Type('z')).transition(st);
    // complete
    let st2 = Transition::CompleteSearch.transition(st);
    assert!(matches!(st2.mode, Mode::Normal));
    let ss = st2.old_search_state.expect("should have old_search_state");
    assert_eq!(&*ss.term, "z");
    assert!(ss.hooked, "search should remain hooked after completion");
}

#[test]
fn scroll_up_down_unhook_search() {
    initialize();
    let mut st = State::default();
    // prime an old_search_state so unhook has something to do
    st.old_search_state = Some(SearchState {
        term: Arc::from("y"),
        idx: 0,
        hooked: true,
    });
    // scroll up
    let st2 = Transition::ScrollUp.transition(st);
    let ss2 = st2
        .old_search_state
        .as_ref()
        .expect("old_search_state still there");
    assert!(!ss2.hooked, "scroll up should unhook search");
    // scroll down
    let st3 = Transition::ScrollDown.transition(st2);
    let ss3 = st3.old_search_state.expect("old_search_state still there");
    assert!(!ss3.hooked, "scroll down should unhook search");
}

#[test]
fn sortmode_cpu_cmp_ordering() {
    initialize();
    // We just check that SortMode::Cpu implements Fn and returns an Ordering
    use crate::process::ProcessData;
    let a = ProcessData {
        pid: Pid::from(1),
        name: Default::default(),
        user: Default::default(),
        cpu_usage: 1.0,
        memory_usage: 0,
        memory_percent: 0.0,
    };
    let b = ProcessData {
        pid: Pid::from(2),
        name: Default::default(),
        user: Default::default(),
        cpu_usage: 2.0,
        memory_usage: 0,
        memory_percent: 0.0,
    };
    // since b.cpu > a.cpu, cmp(a,b) should be Greater (we sort desc)
    assert_eq!(SortMode::Cpu.call((&a, &b)), std::cmp::Ordering::Greater);
    assert_eq!(SortMode::Cpu.call((&b, &a)), std::cmp::Ordering::Less);
}
