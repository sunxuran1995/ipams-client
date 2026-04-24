import React, { useEffect, useState } from "react";
import { TransferItem } from "../components/TransferItem";
import { useTransferStore } from "../stores/transfer";

const REFRESH_INTERVAL = 2000;

export const TransferQueue: React.FC = () => {
  const {
    tasks,
    isLoggedIn,
    username,
    wsConnected,
    loadTasks,
    cancelTask,
    pauseTask,
    resumeTask,
    checkAuth,
    connectWs,
    openLoginPage,
    logout,
  } = useTransferStore();

  const [filter, setFilter] = useState<"all" | "active" | "done">("all");

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

  const filteredTasks = tasks.filter((t) => {
    if (filter === "active")
      return t.status === "pending" || t.status === "running";
    if (filter === "done")
      return (
        t.status === "completed" ||
        t.status === "failed" ||
        t.status === "cancelled"
      );
    return true;
  });

  const activeCount = tasks.filter(
    (t) => t.status === "pending" || t.status === "running"
  ).length;

  const completedCount = tasks.filter((t) => t.status === "completed").length;

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
            gap: 16,
            marginTop: 10,
          }}
        >
          {[
            { label: "全部", value: "all", count: tasks.length },
            { label: "进行中", value: "active", count: activeCount },
            { label: "已完成", value: "done", count: completedCount },
          ].map((tab) => (
            <button
              key={tab.value}
              onClick={() => setFilter(tab.value as typeof filter)}
              style={{
                background:
                  filter === tab.value
                    ? "rgba(99,102,241,0.2)"
                    : "transparent",
                border:
                  filter === tab.value
                    ? "1px solid rgba(99,102,241,0.4)"
                    : "1px solid transparent",
                color: filter === tab.value ? "#6366f1" : "#9ca3af",
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
                        ? "rgba(99,102,241,0.3)"
                        : "rgba(107,114,128,0.2)",
                    color: filter === tab.value ? "#a5b4fc" : "#9ca3af",
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
      </div>

      {/* Task list */}
      <div
        style={{
          flex: 1,
          overflowY: "auto",
          padding: "12px 16px",
          position: "relative",
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
                : "传输队列为空"}
            </div>
          </div>
        ) : (
          filteredTasks.map((task) => (
            <TransferItem
              key={task.upload_id}
              task={task}
              onCancel={cancelTask}
              onPause={pauseTask}
              onResume={resumeTask}
            />
          ))
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
    </div>
  );
};
