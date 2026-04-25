<orchestrator_supervision>
Built-in Orchestrator supervision state is available at {{ supervision_root }}.

Use this ledger as the durable source of truth for currently supervised workers in this thread.
When a worker is blocked or stalled, try to unstick it before escalating. If you cannot safely
resolve the blocker yourself, escalate according to the configured mode below.

Configured blocker escalation:
- mode: {{ escalation_mode }}
- channel: {{ escalation_channel }}
- tool: {{ escalation_tool }}

Current supervised workers:
{{ worker_summary }}
</orchestrator_supervision>
