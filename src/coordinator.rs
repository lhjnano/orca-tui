//! # Coordination Layer
//!
//! The Orca GUI orchestration model: represent the work to be done as a graph
//! of [`Task`]s with dependencies, [`DISPATCH`](Coordinator::dispatch_next)
//! ready tasks to available agents, support a DECISION GATE
//! ([`Coordinator::ask`] / [`Coordinator::answer`] — the coordinator asks an
//! agent a question and awaits its reply), track WORKER DONE, and keep an
//! [`Coordinator::inbox`] of every message that flowed through the system.
//!
//! This module is the **pure model + state machine**. It owns no threads, no
//! PTYs, and no tokio channels; wiring it to live agents is a later concern.
//! All transitions are synchronous and infallible (apart from lookup-style
//! `Option` returns), so the whole thing is trivially unit-testable.
//!
//! ## Lifecycle of a single task
//!
//! ```text
//!   Pending ──dispatch_next──▶ Dispatched ──mark_in_progress──▶ InProgress
//!      │                                                           │
//!      │                                                           ├──report_done──▶ Done(summary)
//!      │                                                           └──report_failed▶ Failed(reason)
//!      │                                                           │
//!      │                                                           └──ask──▶ AwaitingDecision
//!      │                                                                              │
//!      └──────────── (deps not Done ⇒ never dispatched) ◀───answer─── InProgress ◀────┘
//! ```

/// Identifier of a [`Task`] within a [`Coordinator`]. Assigned densely from 0
/// by [`Coordinator::add_task`], so it is also an index into
/// [`Coordinator::tasks`].
pub type TaskId = usize;

/// Lifecycle state of a [`Task`].
///
/// `Done` and `Failed` carry their explanation as a `String` so the UI can
/// render a final summary / error message without a second lookup; both are
/// terminal ([`Coordinator::all_done`] treats them as finished).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Added but not yet dispatched (deps may still be unfinished).
    Pending,
    /// [`Coordinator::dispatch_next`] picked it and assigned an agent, but the
    /// agent has not acknowledged starting yet.
    Dispatched,
    /// Agent has started work on it.
    InProgress,
    /// Coordinator asked the assigned agent a question and is blocked on the
    /// reply; [`Coordinator::answer`] flips it back to `InProgress`.
    AwaitingDecision,
    /// Finished successfully. Carries a human-readable summary.
    Done(String),
    /// Finished unsuccessfully. Carries a human-readable reason.
    Failed(String),
}

impl TaskStatus {
    /// `true` for `Done` or `Failed` — i.e. the task will not transition again
    /// under the current state machine.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Failed(_))
    }
}

/// A unit of work. Created `Pending`, transitions through the state machine
/// as the coordinator drives it.
#[derive(Debug, Clone)]
pub struct Task {
    /// Dense id assigned by [`Coordinator::add_task`]; also an index into the
    /// coordinator's `tasks` vector.
    pub id: TaskId,
    /// Human-readable description of the work; becomes the `prompt` field of
    /// the [`Dispatch`] handed to the agent.
    pub spec: String,
    /// Tasks that must be `Done` before this one can be dispatched. Empty for
    /// a root / fan-out task.
    pub deps: Vec<TaskId>,
    /// Agent currently (or most recently) responsible for this task, if any.
    pub assignee: Option<String>,
    /// Current lifecycle state.
    pub status: TaskStatus,
}

/// A ready-to-run assignment handed off to an agent by
/// [`Coordinator::dispatch_next`].
#[derive(Debug, Clone)]
pub struct Dispatch {
    /// The dispatched task's id.
    pub task_id: TaskId,
    /// The agent chosen for this dispatch.
    pub agent: String,
    /// The prompt to send to the agent — currently the task's `spec` verbatim.
    pub prompt: String,
}

/// A question the coordinator has posed to an agent working on a task.
/// Produced by [`Coordinator::ask`]; the caller is expected to deliver it to
/// the agent out-of-band and later feed the reply back via
/// [`Coordinator::answer`].
#[derive(Debug, Clone)]
pub struct DecisionGate {
    /// The task the question is about.
    pub task_id: TaskId,
    /// The question text.
    pub question: String,
}

/// One line in the coordinator's [`Coordinator::inbox`]. Every public mutation
/// that carries a human-readable message (`report_done`, `answer`, …) appends
/// one of these, so the inbox is a complete audit log of the run.
#[derive(Debug, Clone)]
pub struct InboxMessage {
    /// Logical sender (`assignee`, or a coordinator tag).
    pub from: String,
    /// The task the message concerns, if any (loose / broadcast messages may
    /// have `None`).
    pub task_id: Option<TaskId>,
    /// The message body.
    pub text: String,
}

/// The coordination state machine. Owns the task graph and the message inbox.
///
/// All mutators are synchronous and infallible (lookup-style helpers return
/// `Option`); the intended caller is a single coordinator thread that drives
/// [`dispatch_next`](Self::dispatch_next) in a loop and reacts to agent
/// callbacks. No locking is performed here — external synchronization is the
/// caller's responsibility once wiring lands.
#[derive(Debug, Default, Clone)]
pub struct Coordinator {
    tasks: Vec<Task>,
    inbox: Vec<InboxMessage>,
}

impl Coordinator {
    /// Create an empty coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new `Pending` task depending on `deps`.
    ///
    /// The returned [`TaskId`] is `tasks.len()` *before* the push (i.e. the new
    /// task's index), so ids are dense and stable for the coordinator's
    /// lifetime.
    pub fn add_task<S: Into<String>>(&mut self, spec: S, deps: Vec<TaskId>) -> TaskId {
        let id = self.tasks.len();
        self.tasks.push(Task {
            id,
            spec: spec.into(),
            deps,
            assignee: None,
            status: TaskStatus::Pending,
        });
        id
    }

    /// Pick the next dispatchable task and assign it to an agent.
    ///
    /// A task is dispatchable when it is [`TaskStatus::Pending`] **and** every
    /// id in its `deps` refers to a task whose status is
    /// [`TaskStatus::Done`] (a `Failed` dependency does *not* unblock). The
    /// first such task (lowest id) is paired with the first `available_agent`
    /// and moved to [`TaskStatus::Dispatched`].
    ///
    /// Returns `None` when nothing is ready or no agent is free, leaving state
    /// untouched.
    pub fn dispatch_next(&mut self, available_agents: &[String]) -> Option<Dispatch> {
        let agent = available_agents.first()?;
        let found = self
            .tasks
            .iter()
            .find(|t| t.status == TaskStatus::Pending && self.deps_all_done(&t.deps))?;
        let task_id = found.id;
        // Re-borrow mutably to flip the status and record the assignee. Safe:
        // `task_id` is a dense index that stays valid for the coordinator's
        // lifetime (tasks are never removed).
        let task = self
            .tasks
            .get_mut(task_id)
            .expect("task id is a valid index");
        task.assignee = Some(agent.clone());
        task.status = TaskStatus::Dispatched;
        Some(Dispatch {
            task_id,
            agent: agent.clone(),
            prompt: task.spec.clone(),
        })
    }

    /// Acknowledge that the assigned agent has started work on `id`.
    ///
    /// No-op (but harmless) if the task is already `InProgress`. Panics if `id`
    /// is out of range — callers that cannot prove the id is valid should gate
    /// on [`task`](Self::task) first.
    pub fn mark_in_progress(&mut self, id: TaskId) {
        let task = self
            .tasks
            .get_mut(id)
            .expect("mark_in_progress: task id out of range");
        task.status = TaskStatus::InProgress;
    }

    /// Record a successful completion for `id`.
    ///
    /// Flips the task to [`TaskStatus::Done`] and appends an
    /// [`InboxMessage`] from the task's `assignee` (or `"?"` if it has none)
    /// carrying `summary`. Panics if `id` is out of range.
    pub fn report_done(&mut self, id: TaskId, summary: impl Into<String>) {
        let summary = summary.into();
        let task = self
            .tasks
            .get_mut(id)
            .expect("report_done: task id out of range");
        let from = task.assignee.clone().unwrap_or_else(|| "?".to_string());
        self.inbox.push(InboxMessage {
            from,
            task_id: Some(id),
            text: summary.clone(),
        });
        // The status carries the summary too, so a renderer that only reads
        // `Task::status` can show a final line without consulting the inbox.
        task.status = TaskStatus::Done(summary);
    }

    /// Record a failure for `id`.
    ///
    /// Flips the task to [`TaskStatus::Failed`] with `reason`. Does not append
    /// an inbox message (failures are surfaced via the status, not the chat
    /// log). Panics if `id` is out of range.
    pub fn report_failed(&mut self, id: TaskId, reason: impl Into<String>) {
        let task = self
            .tasks
            .get_mut(id)
            .expect("report_failed: task id out of range");
        task.status = TaskStatus::Failed(reason.into());
    }

    /// Open a decision gate on `id`: the coordinator is blocked on a question
    /// for the task's agent.
    ///
    /// Flips the task to [`TaskStatus::AwaitingDecision`] and returns a
    /// [`DecisionGate`] the caller delivers to the agent out-of-band. Panics
    /// if `id` is out of range.
    pub fn ask(&mut self, id: TaskId, question: impl Into<String>) -> DecisionGate {
        let question = question.into();
        let task = self.tasks.get_mut(id).expect("ask: task id out of range");
        task.status = TaskStatus::AwaitingDecision;
        DecisionGate {
            task_id: id,
            question,
        }
    }

    /// Feed back an agent's reply to an open decision gate on `id`.
    ///
    /// Appends an [`InboxMessage`] (from the task's `assignee`, or `"?"` if it
    /// has none) carrying `reply`, then flips the task back to
    /// [`TaskStatus::InProgress`]. Panics if `id` is out of range.
    ///
    /// Does not require the task to currently be `AwaitingDecision` — callers
    /// that want to enforce that invariant should check
    /// [`task`](Self::task)(id).status first.
    pub fn answer(&mut self, id: TaskId, reply: impl Into<String>) {
        let reply = reply.into();
        let task = self
            .tasks
            .get_mut(id)
            .expect("answer: task id out of range");
        let from = task.assignee.clone().unwrap_or_else(|| "?".to_string());
        self.inbox.push(InboxMessage {
            from,
            task_id: Some(id),
            text: reply,
        });
        task.status = TaskStatus::InProgress;
    }

    /// Borrow the full inbox (audit log) in insertion order.
    #[must_use]
    pub fn inbox(&self) -> &[InboxMessage] {
        &self.inbox
    }

    /// Borrow the full task list in id order.
    #[must_use]
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Borrow a single task by id, or `None` if out of range.
    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// `true` when every task is in a terminal state (`Done` or `Failed`).
    /// Vacuously `true` when there are no tasks.
    #[must_use]
    pub fn all_done(&self) -> bool {
        self.tasks.iter().all(|t| t.status.is_terminal())
    }

    /// `true` iff every id in `deps` resolves to a task whose status is
    /// `Done`. Unknown ids (out-of-range / not-yet-added) count as unresolved,
    /// which is the safe default: a typo'd dep id blocks rather than silently
    /// unblocking.
    fn deps_all_done(&self, deps: &[TaskId]) -> bool {
        deps.iter().all(|&dep| {
            self.tasks
                .get(dep)
                .map(|t| matches!(t.status, TaskStatus::Done(_)))
                .unwrap_or(false)
        })
    }
}

/// Build a [`Coordinator`] from a multi-line spec by fanning each non-empty
/// line out as a parallel root task (no deps).
///
/// This is a deliberately simple **starting heuristic** — a real planner would
/// infer dependencies between subtasks. It exists so a caller can hand the
/// coordinator a free-form todo list and immediately drive
/// [`Coordinator::dispatch_next`] across N agents.
///
/// `agents` is accepted so the returned coordinator's intended fan-out width
/// is documented alongside the plan, but is not otherwise used (assignment
/// happens lazily at dispatch time).
#[must_use]
pub fn plan_from_spec(spec: &str, agents: &[String]) -> Coordinator {
    let mut coord = Coordinator::new();
    for line in spec.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        coord.add_task(trimmed, Vec::new());
    }
    let _ = agents; // documented fan-out width; not consumed here
    coord
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a coordinator with N root tasks (no deps) and a pool of
    /// agent names `a0`, `a1`, … to draw from.
    fn coord_with_agents(specs: &[&str]) -> (Coordinator, Vec<String>) {
        let mut coord = Coordinator::new();
        for s in specs {
            coord.add_task(*s, Vec::new());
        }
        let agents = (0..specs.len()).map(|i| format!("a{i}")).collect();
        (coord, agents)
    }

    #[test]
    fn add_task_assigns_dense_ids_and_starts_pending() {
        let mut coord = Coordinator::new();
        let id0 = coord.add_task("t0", Vec::new());
        let id1 = coord.add_task("t1", Vec::new());
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(coord.tasks().len(), 2);
        assert_eq!(coord.task(0).unwrap().status, TaskStatus::Pending);
        assert_eq!(coord.task(1).unwrap().status, TaskStatus::Pending);
        // task() returns None on out-of-range.
        assert!(coord.task(99).is_none());
    }

    #[test]
    fn dispatch_next_assigns_first_agent_and_moves_to_dispatched() {
        let (mut coord, agents) = coord_with_agents(&["work a", "work b"]);
        let dispatch = coord
            .dispatch_next(&agents)
            .expect("a ready task + a free agent ⇒ Some(Dispatch)");
        assert_eq!(dispatch.task_id, 0);
        assert_eq!(dispatch.agent, "a0");
        assert_eq!(dispatch.prompt, "work a");
        assert_eq!(coord.task(0).unwrap().status, TaskStatus::Dispatched);
        assert_eq!(
            coord.task(0).unwrap().assignee.as_deref(),
            Some("a0"),
            "assignee is recorded on the task"
        );
        // Task 1 untouched.
        assert_eq!(coord.task(1).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn dispatch_next_returns_none_with_no_agents() {
        let (mut coord, _) = coord_with_agents(&["work a"]);
        assert!(coord.dispatch_next(&[]).is_none());
        // State untouched.
        assert_eq!(coord.task(0).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn dispatch_next_returns_none_when_nothing_pending() {
        let (mut coord, agents) = coord_with_agents(&["only"]);
        coord.mark_in_progress(0);
        // Now nothing is Pending.
        assert!(coord.dispatch_next(&agents).is_none());
    }

    #[test]
    fn dependency_gating_blocks_until_dep_done() {
        let mut coord = Coordinator::new();
        let a = coord.add_task("build", Vec::new());
        let b = coord.add_task("test", vec![a]);
        let agents = vec!["agent".to_string()];

        // B depends on A; A is Pending, so dispatch_next should pick A first.
        let d0 = coord.dispatch_next(&agents).expect("A is ready");
        assert_eq!(d0.task_id, a, "A must be dispatched before B");

        // Now only B is Pending, but its dep A is only Dispatched (not Done).
        assert!(
            coord.dispatch_next(&agents).is_none(),
            "B must not dispatch while A is not Done"
        );

        // Drive A to Done.
        coord.mark_in_progress(a);
        coord.report_done(a, "built ok");

        // Now B unblocks.
        let d1 = coord.dispatch_next(&agents).expect("B is now ready");
        assert_eq!(d1.task_id, b);
        assert_eq!(coord.task(b).unwrap().status, TaskStatus::Dispatched);
    }

    #[test]
    fn failed_dependency_does_not_unblock() {
        let mut coord = Coordinator::new();
        let a = coord.add_task("build", Vec::new());
        let b = coord.add_task("test", vec![a]);
        let agents = vec!["agent".to_string()];

        // Dispatch A, then fail it.
        let _ = coord.dispatch_next(&agents);
        coord.mark_in_progress(a);
        coord.report_failed(a, "compile error");

        // B's dep is terminal but Failed, not Done ⇒ still blocked.
        assert!(
            coord.dispatch_next(&agents).is_none(),
            "a Failed dep must not unblock dependents"
        );
        assert_eq!(coord.task(b).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn unknown_dependency_id_blocks() {
        // A typo'd dep id (999) must count as unresolved, not silently pass.
        let mut coord = Coordinator::new();
        let _ = coord.add_task("orphan", vec![999]);
        let agents = vec!["agent".to_string()];
        assert!(
            coord.dispatch_next(&agents).is_none(),
            "unknown dep id must block dispatch"
        );
    }

    #[test]
    fn happy_path_transitions_pending_dispatched_inprogress_done() {
        let (mut coord, agents) = coord_with_agents(&["do thing"]);
        let id = coord.dispatch_next(&agents).unwrap().task_id;
        assert_eq!(coord.task(id).unwrap().status, TaskStatus::Dispatched);

        coord.mark_in_progress(id);
        assert_eq!(coord.task(id).unwrap().status, TaskStatus::InProgress);

        coord.report_done(id, "all good");
        assert!(
            matches!(coord.task(id).unwrap().status, TaskStatus::Done(ref s) if s == "all good"),
            "Done carries the summary"
        );
    }

    #[test]
    fn failed_path_sets_failed_status() {
        let (mut coord, agents) = coord_with_agents(&["do thing"]);
        let id = coord.dispatch_next(&agents).unwrap().task_id;
        coord.mark_in_progress(id);
        coord.report_failed(id, "boom");
        assert!(
            matches!(coord.task(id).unwrap().status, TaskStatus::Failed(ref r) if r == "boom"),
            "Failed carries the reason"
        );
    }

    #[test]
    fn report_done_appends_inbox_message_from_assignee() {
        let (mut coord, agents) = coord_with_agents(&["do thing"]);
        let id = coord.dispatch_next(&agents).unwrap().task_id;
        coord.mark_in_progress(id);
        coord.report_done(id, "shipped");

        let inbox = coord.inbox();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].from, "a0", "from the assigned agent");
        assert_eq!(inbox[0].task_id, Some(id));
        assert_eq!(inbox[0].text, "shipped");
    }

    #[test]
    fn decision_gate_ask_then_answer_round_trip() {
        let (mut coord, agents) = coord_with_agents(&["ambiguous task"]);
        let id = coord.dispatch_next(&agents).unwrap().task_id;
        coord.mark_in_progress(id);

        let gate = coord.ask(id, "which file?");
        assert_eq!(gate.task_id, id);
        assert_eq!(gate.question, "which file?");
        assert_eq!(
            coord.task(id).unwrap().status,
            TaskStatus::AwaitingDecision,
            "ask flips to AwaitingDecision"
        );

        // Inbox untouched by ask itself.
        assert!(coord.inbox().is_empty());

        coord.answer(id, "src/main.rs");
        assert_eq!(
            coord.task(id).unwrap().status,
            TaskStatus::InProgress,
            "answer returns the task to InProgress"
        );
        // answer records the reply in the inbox.
        assert_eq!(coord.inbox().len(), 1);
        assert_eq!(coord.inbox()[0].text, "src/main.rs");
        assert_eq!(coord.inbox()[0].task_id, Some(id));
        assert_eq!(coord.inbox()[0].from, "a0");
    }

    #[test]
    fn inbox_accumulates_across_messages() {
        let (mut coord, agents) = coord_with_agents(&["t0", "t1"]);
        let d0 = coord.dispatch_next(&agents).unwrap().task_id;
        let d1 = coord.dispatch_next(&agents).unwrap().task_id;
        coord.mark_in_progress(d0);
        coord.mark_in_progress(d1);
        coord.answer(d0, "q0 reply");
        coord.report_done(d0, "done0");
        coord.answer(d1, "q1 reply");
        coord.report_done(d1, "done1");

        assert_eq!(coord.inbox().len(), 4, "two answers + two dones");
    }

    #[test]
    fn all_done_true_only_when_every_task_terminal() {
        let (mut coord, agents) = coord_with_agents(&["t0", "t1"]);
        // Two Pending tasks ⇒ not done.
        assert!(!coord.all_done());

        // Drive one to Done, the other still Pending ⇒ still not done.
        let d0 = coord.dispatch_next(&agents).unwrap().task_id;
        coord.mark_in_progress(d0);
        coord.report_done(d0, "ok");
        assert!(!coord.all_done(), "one task still Pending");

        // Fail the other ⇒ all terminal.
        let d1 = coord.dispatch_next(&agents).unwrap().task_id;
        coord.report_failed(d1, "nope");
        assert!(coord.all_done(), "Done + Failed ⇒ all_done");
    }

    #[test]
    fn all_done_vacuously_true_with_no_tasks() {
        assert!(Coordinator::new().all_done());
    }

    #[test]
    fn default_equals_new() {
        assert_eq!(Coordinator::new().tasks().len(), 0);
        assert_eq!(Coordinator::default().tasks().len(), 0);
    }

    #[test]
    fn plan_from_spec_yields_one_task_per_non_empty_line() {
        let spec = "first\n\n  \nsecond\nthird";
        let agents = vec!["a".to_string(), "b".to_string()];
        let coord = plan_from_spec(spec, &agents);

        assert_eq!(coord.tasks().len(), 3, "blank/whitespace lines dropped");
        // Lines are trimmed.
        assert_eq!(coord.task(0).unwrap().spec, "first");
        assert_eq!(coord.task(1).unwrap().spec, "second");
        assert_eq!(coord.task(2).unwrap().spec, "third");
        // All roots, all Pending.
        for t in coord.tasks() {
            assert!(t.deps.is_empty());
            assert_eq!(t.status, TaskStatus::Pending);
        }
        // plan_from_spec output is drivable by dispatch_next.
        let mut coord = coord;
        let d = coord
            .dispatch_next(&agents)
            .expect("plan output is dispatchable");
        assert_eq!(d.task_id, 0);
        assert_eq!(d.prompt, "first");
    }

    #[test]
    fn plan_from_spec_empty_spec_yields_no_tasks() {
        let coord = plan_from_spec("", &[]);
        assert!(coord.tasks().is_empty());
        assert!(coord.all_done(), "vacuously done");
    }

    #[test]
    fn dispatch_picks_lowest_id_ready_task_first() {
        // Three independent tasks; dispatch_next should always return them in
        // id order across successive calls.
        let (mut coord, agents) = coord_with_agents(&["t0", "t1", "t2"]);
        let first = coord.dispatch_next(&agents).unwrap().task_id;
        let second = coord.dispatch_next(&agents).unwrap().task_id;
        let third = coord.dispatch_next(&agents).unwrap().task_id;
        assert_eq!((first, second, third), (0, 1, 2));
        // No more pending tasks.
        assert!(coord.dispatch_next(&agents).is_none());
    }

    #[test]
    fn dispatch_skips_dispatched_and_in_progress_when_picking_next() {
        let (mut coord, agents) = coord_with_agents(&["t0", "t1"]);
        // Dispatch t0 manually mark it in-progress, then ensure dispatch_next
        // skips it and picks t1.
        let d0 = coord.dispatch_next(&agents).unwrap().task_id;
        coord.mark_in_progress(d0);
        let d1 = coord.dispatch_next(&agents).unwrap().task_id;
        assert_eq!(d1, 1, "InProgress task must not be re-picked");
    }
}
