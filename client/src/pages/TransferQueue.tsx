import React, { useEffect, useState, useMemo, useCallback, useRef, CSSProperties, ReactElement } from "react";
import { List } from "react-window";
import { TransferItem } from "../components/TransferItem";
import { useTransferStore, TransferTask } from "../stores/transfer";

const REFRESH_INTERVAL = 5000; // Reduced polling frequency — WS handles real-time updates
const ITEM_HEIGHT = 120; // 每个 TransferItem 的固定高度（含 margin）

// react-window v2 的 rowComponent 需要是独立组件
interface VirtualRowProps {
  tasks: TransferTask[];
  onCancel: (uploadId: string) => void;
  onPause: (uploadId: string) => void;
  onResume: (uploadId: string) => void;
}

function VirtualRow({ ariaAttributes, index, style, tasks, onCancel, onPause, onResume }: {
  ariaAttributes: { "aria-posinset": number; "aria-setsize": number; role: "listitem" };
  index: number;
  style: CSSProperties;
} & VirtualRowProps): ReactElement | null {
  const task = tasks[index];
  if (!task) return null;
  return (
    <div style={{ ...style, padding: "0 16px" }} {...ariaAttributes}>
      <TransferItem
        task={task}
        onCancel={onCancel}
        onPause={onPause}
        onResume={onResume}
      />
    </div>
  );
}

export const TransferQueue: React.FC = () => {
  const {
    tasks,
    isLoggedIn,
    username,
    wsConnected,
    loginSuccessToast,
    loadTasks,
    cancelTask,
    pauseTask,
    resumeTask,
    pauseAll,
    resumeAll,
    cancelAll,
    retryAll,
    clearCompleted,
    checkAuth,
    connectWs,
    openLoginPage,
    logout,
  } = useTransferStore();

  const [filter, setFilter] = useState<"all" | "active" | "done" | "failed" | "cancelled">("all");
  const [listHeight, setListHeight] = useState(400);
  const listContainerRef = useRef<HTMLDivElement | null>(null);

  // 监听容器高度变化
  useEffect(() => {
    const el = listContainerRef.current;
    if (!el) return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        setListHeight(entry.contentRect.height);
      }
    });
    observer.observe(el);
    setListHeight(el.clientHeight);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    checkAuth();
    loadTasks();
    connectWs();

    // Periodic refresh to sync state
    const interval = setInterval(() => {
      if (useTransferStore.getState().isLoggedIn) {
        loadTasks();
      }
    }, REFRESH_INTERVAL);

    return () => clearInterval(interval);
  }, []);

  const filteredTasks = useMemo(() => tasks.filter((t) => {
    if (filter === "active")
      return t.status === "pending" || t.status === "running" || t.status === "paused";
    if (filter === "done")
      return t.status === "completed";
    if (filter === "failed")
      return t.status === "failed";
    if (filter === "cancelled")
      return t.status === "cancelled";
    return true;
  }), [tasks, filter]);

  const activeCount = useMemo(() => tasks.filter(
    (t) => t.status === "pending" || t.status === "running" || t.status === "paused"
  ).length, [tasks]);

  const completedCount = useMemo(() => tasks.filter((t) => t.status === "completed").length, [tasks]);

  const failedCount = useMemo(() => tasks.filter((t) => t.status === "failed").length, [tasks]);

  const cancelledCount = useMemo(() => tasks.filter((t) => t.status === "cancelled").length, [tasks]);

  const pausedCount = useMemo(() => tasks.filter((t) => t.status === "paused").length, [tasks]);

  const hasCancellable = useMemo(() => tasks.some(
    (t) => t.status === "running" || t.status === "pending" || t.status === "paused"
  ), [tasks]);

  const hasClearable = useMemo(() => tasks.some(
    (t) => t.status === "completed" || t.status === "cancelled" || t.status === "failed"
  ), [tasks]);

  return (
    <div
      style={{
        height: "100vh",
        display: "flex",
        flexDirection: "column",
        background: "#0f0f1a",
      }}
    >
      {/* Header */}
      <div
        style={{
          padding: "16px 20px 12px",
          background: "#131320",
          borderBottom: "1px solid #2d2d44",
          userSelect: "none",
          WebkitAppRegion: "drag",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
            {/* Logo */}
            <div
              style={{
                width: 28,
                height: 28,
                borderRadius: 8,
                background: "linear-gradient(135deg, #6366f1, #8b5cf6)",
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                fontSize: 14,
                fontWeight: 700,
                color: "#fff",
                flexShrink: 0,
              }}
            >
              I
            </div>
            <div>
              <div style={{ fontSize: 14, fontWeight: 700, color: "#e5e7eb" }}>
                IPAMS 传输队列
              </div>
              <div style={{ fontSize: 11, color: "#6b7280" }}>
                {activeCount > 0 ? (
                  <span style={{ color: "#6366f1" }}>
                    {activeCount} 个任务进行中
                  </span>
                ) : (
                  <span>无活动任务</span>
                )}
              </div>
            </div>
          </div>

          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: 12,
              WebkitAppRegion: "no-drag",
            }}
          >
            {/* WS status indicator */}
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 5,
                fontSize: 11,
                color: wsConnected ? "#10b981" : "#ef4444",
              }}
            >
              <div
                style={{
                  width: 6,
                  height: 6,
                  borderRadius: "50%",
                  background: wsConnected ? "#10b981" : "#ef4444",
                }}
              />
              {wsConnected ? "已连接" : "未连接"}
            </div>

            {/* Auth status */}
            {!isLoggedIn ? (
              <button
                onClick={openLoginPage}
                style={{
                  background: "linear-gradient(135deg, #6366f1, #8b5cf6)",
                  border: "none",
                  color: "#fff",
                  borderRadius: 7,
                  padding: "5px 14px",
                  fontSize: 12,
                  fontWeight: 600,
                  cursor: "pointer",
                }}
              >
                登录
              </button>
            ) : (
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <div
                  style={{
                    fontSize: 11,
                    color: "#10b981",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                  }}
                >
                  <div style={{
                    width: 22, height: 22, borderRadius: "50%",
                    background: "linear-gradient(135deg, #6366f1, #8b5cf6)",
                    display: "flex", alignItems: "center", justifyContent: "center",
                    fontSize: 11, fontWeight: 700, color: "#fff",
                  }}>
                    {username ? username[0].toUpperCase() : "U"}
                  </div>
                  <span style={{ color: "#e5e7eb", fontSize: 12 }}>
                    {username ?? "已登录"}
                  </span>
                </div>
                <button
                  onClick={logout}
                  style={{
                    background: "transparent",
                    border: "1px solid #374151",
                    color: "#9ca3af",
                    borderRadius: 6,
                    padding: "3px 10px",
                    fontSize: 11,
                    cursor: "pointer",
                    transition: "all 0.2s",
                  }}
                  onMouseEnter={(e) => { e.currentTarget.style.borderColor = "#ef4444"; e.currentTarget.style.color = "#ef4444"; }}
                  onMouseLeave={(e) => { e.currentTarget.style.borderColor = "#374151"; e.currentTarget.style.color = "#9ca3af"; }}
                >
                  注销
                </button>
              </div>
            )}
          </div>
        </div>

        {/* Stats row */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            marginTop: 10,
          }}
        >
          {/* Filter tabs */}
          <div style={{ display: "flex", gap: 8, flex: 1 }}>
            {[
              { label: "全部", value: "all", count: tasks.length },
              { label: "进行中", value: "active", count: activeCount },
              { label: "已完成", value: "done", count: completedCount },
              { label: "失败", value: "failed", count: failedCount, isError: true },
              { label: "已取消", value: "cancelled", count: cancelledCount, isMuted: true },
            ].map((tab) => (
              <button
                key={tab.value}
                onClick={() => setFilter(tab.value as typeof filter)}
                style={{
                  background:
                    filter === tab.value
                      ? tab.isError
                        ? "rgba(239,68,68,0.15)"
                        : tab.isMuted
                        ? "rgba(107,114,128,0.15)"
                        : "rgba(99,102,241,0.2)"
                      : "transparent",
                  border:
                    filter === tab.value
                      ? tab.isError
                        ? "1px solid rgba(239,68,68,0.4)"
                        : tab.isMuted
                        ? "1px solid rgba(107,114,128,0.4)"
                        : "1px solid rgba(99,102,241,0.4)"
                      : tab.isError && tab.count > 0
                      ? "1px solid rgba(239,68,68,0.25)"
                      : "1px solid transparent",
                  color:
                    filter === tab.value
                      ? tab.isError
                        ? "#ef4444"
                        : tab.isMuted
                        ? "#9ca3af"
                        : "#6366f1"
                      : tab.isError && tab.count > 0
                      ? "#f87171"
                      : "#9ca3af",
                  borderRadius: 6,
                  padding: "4px 12px",
                  fontSize: 12,
                  fontWeight: 500,
                  cursor: "pointer",
                  transition: "all 0.2s",
                  WebkitAppRegion: "no-drag",
                }}
              >
                {tab.label}
                {tab.count > 0 && (
                  <span
                    style={{
                      marginLeft: 6,
                      background:
                        filter === tab.value
                          ? tab.isError
                            ? "rgba(239,68,68,0.25)"
                            : tab.isMuted
                            ? "rgba(107,114,128,0.25)"
                            : "rgba(99,102,241,0.3)"
                          : tab.isError
                          ? "rgba(239,68,68,0.15)"
                          : "rgba(107,114,128,0.2)",
                      color:
                        filter === tab.value
                          ? tab.isError
                            ? "#fca5a5"
                            : tab.isMuted
                            ? "#d1d5db"
                            : "#a5b4fc"
                          : tab.isError
                          ? "#f87171"
                          : "#9ca3af",
                      borderRadius: 10,
                      padding: "1px 6px",
                      fontSize: 11,
                    }}
                  >
                    {tab.count}
                  </span>
                )}
              </button>
            ))}
          </div>

          {/* Bulk action buttons */}
          {isLoggedIn && (
            <div
              style={{
                display: "flex",
                gap: 6,
                WebkitAppRegion: "no-drag",
              }}
            >
              {activeCount > 0 && (
                <button
                  onClick={pauseAll}
                  title="全部暂停"
                  style={{
                    background: "transparent",
                    border: "1px solid #374151",
                    color: "#9ca3af",
                    borderRadius: 6,
                    padding: "3px 10px",
                    fontSize: 12,
                    cursor: "pointer",
                    transition: "all 0.2s",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.borderColor = "#f59e0b";
                    e.currentTarget.style.color = "#f59e0b";
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.borderColor = "#374151";
                    e.currentTarget.style.color = "#9ca3af";
                  }}
                >
                  {/* pause icon */}
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor">
                    <rect x="6" y="4" width="4" height="16" rx="1" />
                    <rect x="14" y="4" width="4" height="16" rx="1" />
                  </svg>
                  全部暂停
                </button>
              )}

              {pausedCount > 0 && (
                <button
                  onClick={resumeAll}
                  title="全部开始"
                  style={{
                    background: "transparent",
                    border: "1px solid #374151",
                    color: "#9ca3af",
                    borderRadius: 6,
                    padding: "3px 10px",
                    fontSize: 12,
                    cursor: "pointer",
                    transition: "all 0.2s",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.borderColor = "#10b981";
                    e.currentTarget.style.color = "#10b981";
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.borderColor = "#374151";
                    e.currentTarget.style.color = "#9ca3af";
                  }}
                >
                  {/* play icon */}
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor">
                    <path d="M8 5v14l11-7z" />
                  </svg>
                  全部开始
                </button>
              )}

              {hasCancellable && (
                <button
                  onClick={cancelAll}
                  title="全部取消"
                  style={{
                    background: "transparent",
                    border: "1px solid #374151",
                    color: "#9ca3af",
                    borderRadius: 6,
                    padding: "3px 10px",
                    fontSize: 12,
                    cursor: "pointer",
                    transition: "all 0.2s",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.borderColor = "#ef4444";
                    e.currentTarget.style.color = "#ef4444";
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.borderColor = "#374151";
                    e.currentTarget.style.color = "#9ca3af";
                  }}
                >
                  {/* stop icon */}
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor">
                    <rect x="4" y="4" width="16" height="16" rx="2" />
                  </svg>
                  全部取消
                </button>
              )}

              {failedCount > 0 && (
                <button
                  onClick={retryAll}
                  title="全部重试"
                  style={{
                    background: "transparent",
                    border: "1px solid rgba(239,68,68,0.35)",
                    color: "#f87171",
                    borderRadius: 6,
                    padding: "3px 10px",
                    fontSize: 12,
                    cursor: "pointer",
                    transition: "all 0.2s",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.borderColor = "#ef4444";
                    e.currentTarget.style.color = "#ef4444";
                    e.currentTarget.style.background = "rgba(239,68,68,0.08)";
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.borderColor = "rgba(239,68,68,0.35)";
                    e.currentTarget.style.color = "#f87171";
                    e.currentTarget.style.background = "transparent";
                  }}
                >
                  {/* retry icon */}
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                    <path d="M1 4v6h6" />
                    <path d="M3.51 15a9 9 0 1 0 .49-4.5" />
                  </svg>
                  全部重试
                </button>
              )}

              {hasClearable && (
                <button
                  onClick={clearCompleted}
                  title="清空已完成/已取消/已失败的记录"
                  style={{
                    background: "transparent",
                    border: "1px solid #374151",
                    color: "#9ca3af",
                    borderRadius: 6,
                    padding: "3px 10px",
                    fontSize: 12,
                    cursor: "pointer",
                    transition: "all 0.2s",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.borderColor = "#6b7280";
                    e.currentTarget.style.color = "#e5e7eb";
                    e.currentTarget.style.background = "rgba(107,114,128,0.1)";
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.borderColor = "#374151";
                    e.currentTarget.style.color = "#9ca3af";
                    e.currentTarget.style.background = "transparent";
                  }}
                >
                  {/* trash icon */}
                  <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                    <polyline points="3 6 5 6 21 6" />
                    <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                  </svg>
                  清空列表
                </button>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Task list */}
      <div
        ref={listContainerRef}
        style={{
          flex: 1,
          position: "relative",
          overflow: "hidden",
        }}
      >
        {/* 未登录遮罩 */}
        {!isLoggedIn && (
          <div
            style={{
              position: "absolute",
              inset: 0,
              background: "rgba(15,15,26,0.85)",
              backdropFilter: "blur(4px)",
              display: "flex",
              flexDirection: "column",
              alignItems: "center",
              justifyContent: "center",
              gap: 14,
              zIndex: 10,
            }}
          >
            <div style={{ fontSize: 32 }}>🔒</div>
            <div style={{ fontSize: 14, fontWeight: 600, color: "#e5e7eb" }}>
              请先登录
            </div>
            <div style={{ fontSize: 12, color: "#6b7280", textAlign: "center", maxWidth: 220 }}>
              登录后才能查看传输队列和发起上传任务
            </div>
            <button
              onClick={openLoginPage}
              style={{
                marginTop: 4,
                background: "linear-gradient(135deg, #6366f1, #8b5cf6)",
                border: "none",
                color: "#fff",
                borderRadius: 8,
                padding: "8px 24px",
                fontSize: 13,
                fontWeight: 600,
                cursor: "pointer",
              }}
            >
              前往登录
            </button>
          </div>
        )}

        {filteredTasks.length === 0 ? (
          <div
            style={{
              height: "100%",
              display: "flex",
              flexDirection: "column",
              alignItems: "center",
              justifyContent: "center",
              color: "#4b5563",
              gap: 12,
            }}
          >
            <svg
              width="48"
              height="48"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
            >
              <path d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
            </svg>
            <div style={{ fontSize: 14 }}>
              {filter === "active"
                ? "没有进行中的任务"
                : filter === "done"
                ? "没有已完成的任务"
                : filter === "failed"
                ? "没有失败的任务"
                : filter === "cancelled"
                ? "没有已取消的任务"
                : "传输队列为空"}
            </div>
          </div>
        ) : (
          <List
            rowComponent={VirtualRow}
            rowCount={filteredTasks.length}
            rowHeight={ITEM_HEIGHT}
            rowProps={{
              tasks: filteredTasks,
              onCancel: cancelTask,
              onPause: pauseTask,
              onResume: resumeTask,
            }}
            overscanCount={10}
            style={{ height: listHeight, width: "100%" }}
          />
        )}
      </div>

      {/* Footer */}
      <div
        style={{
          padding: "8px 16px",
          background: "#131320",
          borderTop: "1px solid #2d2d44",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          fontSize: 11,
          color: "#4b5563",
          userSelect: "none",
        }}
      >
        <span>IPAMS 传输客户端 v0.1.0</span>
        <span>ws://127.0.0.1:17892/ws</span>
      </div>

      {/* 登录成功 Toast */}
      {loginSuccessToast && (
        <div
          style={{
            position: "fixed",
            bottom: 48,
            left: "50%",
            transform: "translateX(-50%)",
            background: "linear-gradient(135deg, #065f46, #047857)",
            border: "1px solid #10b981",
            color: "#d1fae5",
            borderRadius: 10,
            padding: "10px 20px",
            fontSize: 13,
            fontWeight: 600,
            display: "flex",
            alignItems: "center",
            gap: 8,
            boxShadow: "0 4px 20px rgba(16,185,129,0.3)",
            zIndex: 100,
            whiteSpace: "nowrap",
            animation: "fadeInUp 0.3s ease",
          }}
        >
          <span style={{ fontSize: 16 }}>✓</span>
          登录成功{username ? `，欢迎 ${username}` : ""}
        </div>
      )}
    </div>
  );
};
