"use client";

import { useState } from "react";

/**
 * Shared approve/deny presentational card. Two current callers with two
 * different resolution mechanisms:
 *  - AgentTools.tsx renders this from useInterrupt's `render`, whose
 *    `resolve()` resumes a paused Strands tool call on the backend.
 *  - useSandboxFunctionBridge.tsx renders this standalone (fixed-position
 *    overlay, no chat message slot), resolving a local Promise instead —
 *    there is no backend checkpoint to resume for a sandbox-function call.
 * This component only renders and reports a decision; it has no opinion on
 * what approving actually does.
 */
export default function ApprovalCard({
  toolName,
  summary,
  code,
  allowReason,
  onApprove,
  onDeny,
}: {
  toolName: string;
  summary: string;
  code?: boolean;
  // When set, Deny carries an optional free-text reason back to the caller.
  allowReason?: boolean;
  onApprove: () => Promise<unknown>;
  onDeny: (reason?: string) => Promise<unknown>;
}) {
  const [reason, setReason] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const approve = async () => {
    setSubmitting(true);
    try {
      await onApprove();
    } finally {
      setSubmitting(false);
    }
  };

  const deny = async () => {
    setSubmitting(true);
    try {
      await onDeny(allowReason ? reason || undefined : undefined);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="approval-card">
      <div className="approval-card-tool">Approve {toolName}?</div>
      {code ? (
        <pre className="approval-card-code">{summary}</pre>
      ) : (
        <div className="approval-card-summary">{summary}</div>
      )}
      {allowReason && (
        <input
          className="approval-card-reason"
          type="text"
          placeholder="Reason (optional, sent if you deny)"
          value={reason}
          onChange={(e) => setReason(e.target.value)}
        />
      )}
      <div className="approval-card-actions">
        <button className="approval-card-approve" disabled={submitting} onClick={approve}>
          Approve
        </button>
        <button className="approval-card-deny" disabled={submitting} onClick={deny}>
          Deny
        </button>
      </div>
    </div>
  );
}
