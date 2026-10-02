Goal round {round}/{max} — continue working toward the goal: {objective}

Work on the next step now — do not just summarize or ask what to do.
If something blocks progress, call UpdateGoal with `blocker` set to the
current obstacle. Report the same blocker on consecutive rounds; once it
persists across rounds the goal may be marked `blocked`. Call
UpdateGoal with `status: "complete"` when the objective is genuinely met —
the chain stops there.
