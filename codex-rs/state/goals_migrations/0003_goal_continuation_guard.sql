ALTER TABLE thread_goal_continuation_deferrals
ADD COLUMN kind TEXT NOT NULL DEFAULT 'fork' CHECK (kind IN ('fork', 'suspected_stall'));
ALTER TABLE thread_goal_continuation_deferrals ADD COLUMN goal_id TEXT;

CREATE TABLE thread_goal_continuation_guard_state (
    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE,
    goal_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (length(state) <= 4096)
);
