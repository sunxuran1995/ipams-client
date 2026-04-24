import React from "react";
import { TransferTask } from "../stores/transfer";

interface Props {
  task: TransferTask;
  onCancel: (uploadId: string) => void;
  onPause: (uploadId: string) => void;
  onResume: (uploadId: string) => void;
}

const STATUS_CONFIG: Record<
  string,
  { label: string; color: string; bg: string }
> = {
  pending: { label: "等待中", color: "#f59e0b", bg: "rgba(245,158,11,0.12)" },
  running: { label: "上传中", color: "#6366f1", bg: "rgba(99,102,241,0.12)" },
  paused: { label: "已暂停", color: "#9ca3af", bg: "rgba(156,163,175,0.12)" },
  completed: {
    label: "已完成",
    color: "#10b981",
    bg: "rgba(16,185,129,0.12)",
  },
  failed: { label: "失败", color: "#ef4444", bg: "rgba(239,68,68,0.12)" },
  cancelled: {
    label: "已取消",
    color: "#6b7280",
    bg: "rgba(107,114,128,0.12)",
  },
};

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export const TransferItem: React.FC<Props> = ({ task, onCancel, onPause, onResume }) => {
  const percent =
    task.total_chunks > 0
      ? Math.round((task.uploaded_chunks / task.total_chunks) * 100)
      : 0;

  const statusCfg = STATUS_CONFIG[task.status] ?? STATUS_CONFIG.pending;
  const canCancel = task.status === "pending" || task.status === "running" || task.status === "paused";
  const canPause = task.status === "running" || task.status === "pending";
  const canResume = task.status === "paused";

  return (
    <div
      style={{
        background: "#1a1a2e",
        border: "1px solid #2d2d44",
        borderRadius: 10,
        padding: "14px 16px",
        marginBottom: 10,
        transition: "border-color 0.2s",
      }}
      onMouseEnter={(e) => {
        (e.currentTarget as HTMLDivElement).style.borderColor = "#6366f1";
      }}
      onMouseLeave={(e) => {
        (e.currentTarget as HTMLDivElement).style.borderColor = "#2d2d44";
      }}
    >
      {/* Top row: filename + status badge + cancel */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: 10,
        }}
      >
        <div style={{ flex: 1, minWidth: 0 }}>
          <div
            style={{
              fontSize: 14,
              fontWeight: 600,
              color: "#e5e7eb",
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
            title={task.filename}
          >
            {task.filename}
          </div>
          <div style={{ fontSize: 12, color: "#6b7280", marginTop: 2 }}>
            {formatSize(task.file_size)} · {task.total_chunks} 分片 ·{" "}
            {formatTime(task.created_at)}
          </div>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 8, marginLeft: 12 }}>
          <span
            style={{
              fontSize: 12,
              fontWeight: 500,
              color: statusCfg.color,
              background: statusCfg.bg,
              padding: "3px 10px",
              borderRadius: 20,
              whiteSpace: "nowrap",
            }}
          >
            {statusCfg.label}
          </span>

          {canPause && (
            <button
              onClick={() => onPause(task.upload_id)}
              style={{
                background: "transparent",
                border: "1px solid #374151",
                color: "#9ca3af",
                borderRadius: 6,
                padding: "3px 10px",
                fontSize: 12,
                cursor: "pointer",
                transition: "all 0.2s",
              }}
              onMouseEnter={(e) => { e.currentTarget.style.borderColor = "#f59e0b"; e.currentTarget.style.color = "#f59e0b"; }}
              onMouseLeave={(e) => { e.currentTarget.style.borderColor = "#374151"; e.currentTarget.style.color = "#9ca3af"; }}
            >
              暂停
            </button>
          )}

          {canResume && (
            <button
              onClick={() => onResume(task.upload_id)}
              style={{
                background: "transparent",
                border: "1px solid #374151",
                color: "#9ca3af",
                borderRadius: 6,
                padding: "3px 10px",
                fontSize: 12,
                cursor: "pointer",
                transition: "all 0.2s",
              }}
              onMouseEnter={(e) => { e.currentTarget.style.borderColor = "#10b981"; e.currentTarget.style.color = "#10b981"; }}
              onMouseLeave={(e) => { e.currentTarget.style.borderColor = "#374151"; e.currentTarget.style.color = "#9ca3af"; }}
            >
              继续
            </button>
          )}

          {canCancel && (
            <button
              onClick={() => onCancel(task.upload_id)}
              style={{
                background: "transparent",
                border: "1px solid #374151",
                color: "#9ca3af",
                borderRadius: 6,
                padding: "3px 10px",
                fontSize: 12,
                cursor: "pointer",
                transition: "all 0.2s",
              }}
              onMouseEnter={(e) => { e.currentTarget.style.borderColor = "#ef4444"; e.currentTarget.style.color = "#ef4444"; }}
              onMouseLeave={(e) => { e.currentTarget.style.borderColor = "#374151"; e.currentTarget.style.color = "#9ca3af"; }}
            >
              取消
            </button>
          )}
        </div>
      </div>

      {/* Progress bar */}
      <div
        style={{
          height: 6,
          background: "#2d2d44",
          borderRadius: 3,
          overflow: "hidden",
        }}
      >
        <div
          style={{
            height: "100%",
            width: `${percent}%`,
            background:
              task.status === "completed"
                ? "#10b981"
                : task.status === "failed"
                ? "#ef4444"
                : task.status === "cancelled"
                ? "#6b7280"
                : "linear-gradient(90deg, #6366f1, #8b5cf6)",
            borderRadius: 3,
            transition: "width 0.3s ease",
          }}
        />
      </div>

      {/* Progress text */}
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          marginTop: 6,
          fontSize: 12,
          color: "#6b7280",
        }}
      >
        <span>
          {task.uploaded_chunks}/{task.total_chunks} 分片
        </span>
        <span style={{ color: task.status === "running" ? "#6366f1" : "#6b7280" }}>
          {percent}%
        </span>
      </div>

      {/* Error message */}
      {task.error && (
        <div
          style={{
            marginTop: 8,
            padding: "6px 10px",
            background: "rgba(239,68,68,0.08)",
            border: "1px solid rgba(239,68,68,0.2)",
            borderRadius: 6,
            fontSize: 12,
            color: "#ef4444",
          }}
        >
          {task.error}
        </div>
      )}
    </div>
  );
};
