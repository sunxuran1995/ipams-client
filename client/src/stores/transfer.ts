import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";

export type TaskStatus =
  | "pending"
  | "running"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export interface TransferTask {
  upload_id: string;
  filename: string;
  file_size: number;
  total_chunks: number;
  uploaded_chunks: number;
  status: TaskStatus;
  error?: string;
  created_at: number;
  retry_count?: number;
}

export interface AppConfig {
  api_url: string;
  web_url: string;
  ws_port: number;
  max_concurrent_chunks: number;
  chunk_size: number;
}

interface WsProgressMessage {
  type: string;
  upload_id: string;
  uploaded_chunks?: number;
  total_chunks?: number;
  percent?: number;
  status?: string;
}

interface TransferStore {
  tasks: TransferTask[];
  config: AppConfig | null;
  isLoggedIn: boolean;
  username: string | null;
  wsConnected: boolean;
  wsClient: WebSocket | null;
  loginSuccessToast: boolean;

  // Actions
  loadTasks: () => Promise<void>;
  cancelTask: (uploadId: string) => Promise<void>;
  pauseTask: (uploadId: string) => Promise<void>;
  resumeTask: (uploadId: string) => Promise<void>;
  pauseAll: () => Promise<void>;
  resumeAll: () => Promise<void>;
  cancelAll: () => Promise<void>;
  retryAll: () => Promise<void>;
  clearCompleted: () => Promise<void>;
  loadConfig: () => Promise<void>;
  checkAuth: () => Promise<void>;
  connectWs: () => void;
  disconnectWs: () => void;
  openLoginPage: () => Promise<void>;
  logout: () => Promise<void>;

  // Internal
  _updateTaskFromWs: (msg: WsProgressMessage) => void;
}

// ── 防抖 loadTasks：避免短时间内多次 WS 消息触发重复全量刷新 ──────────────
// 大量文件上传时（60000+），loadTasks 通过 IPC 拉取全部任务列表，
// 序列化 + 传输 + 反序列化开销巨大，必须严格控制调用频率。
let _loadTasksTimer: ReturnType<typeof setTimeout> | null = null;
let _loadTasksInFlight = false;

function _debouncedLoadTasks(get: () => TransferStore, delayMs = 500) {
  if (_loadTasksTimer) clearTimeout(_loadTasksTimer);
  _loadTasksTimer = setTimeout(() => {
    _loadTasksTimer = null;
    if (!_loadTasksInFlight) {
      get().loadTasks();
    }
  }, delayMs);
}

// ── WS 消息批量合并：收集一帧内的所有消息，合并后一次性更新 state ──────────
// 避免每条 WS 消息都触发 zustand set() → React 重渲染
let _pendingWsMessages: WsProgressMessage[] = [];
let _wsFlushScheduled = false;

function _scheduleWsFlush(get: () => TransferStore) {
  if (_wsFlushScheduled) return;
  _wsFlushScheduled = true;
  requestAnimationFrame(() => {
    _wsFlushScheduled = false;
    const messages = _pendingWsMessages;
    _pendingWsMessages = [];
    if (messages.length === 0) return;
    _applyWsBatch(get, messages);
  });
}

function _applyWsBatch(get: () => TransferStore, messages: WsProgressMessage[]) {
  const store = get();
  let needsFullReload = false;

  // 按 upload_id 合并：同一个 task 的多条 progress 只保留最后一条
  const mergedByUploadId = new Map<string, WsProgressMessage>();
  const otherMessages: WsProgressMessage[] = [];

  for (const msg of messages) {
    // 需要全量刷新的消息类型
    if (
      msg.type === "all_paused" ||
      msg.type === "all_resumed" ||
      msg.type === "queue_cleared" ||
      msg.type === "all_uploads_complete" ||
      msg.type === "messages_lagged" ||
      msg.type === "task_reinit"
    ) {
      needsFullReload = true;
      continue;
    }

    // upload_queued / upload_instant 表示有新任务入队
    // 不立即 reload，累积后由 all_uploads_complete 或定时刷新处理
    // 避免 60000 文件 init 阶段每个文件都触发 loadTasks
    if (msg.type === "upload_queued" || msg.type === "upload_instant") {
      needsFullReload = true;
      continue;
    }

    if (msg.upload_id) {
      // 同一个 upload_id 的消息，后面的覆盖前面的（progress 取最新值）
      const existing = mergedByUploadId.get(msg.upload_id);
      if (existing) {
        // 状态变更优先级：completed > cancelled > paused > running > pending
        // 如果新消息是 completed/cancelled，覆盖旧的 progress
        if (msg.type === "completed" || msg.type === "task_cancelled") {
          mergedByUploadId.set(msg.upload_id, msg);
        } else if (existing.type !== "completed" && existing.type !== "task_cancelled") {
          mergedByUploadId.set(msg.upload_id, msg);
        }
      } else {
        mergedByUploadId.set(msg.upload_id, msg);
      }
    } else {
      otherMessages.push(msg);
    }
  }

  // 应用合并后的消息到 tasks 数组
  if (mergedByUploadId.size > 0) {
    useTransferStore.setState((state) => {
      const tasks = [...state.tasks];
      let changed = false;

      for (const [uploadId, msg] of mergedByUploadId) {
        const idx = tasks.findIndex((t) => t.upload_id === uploadId);
        if (idx === -1) {
          // 未知任务：只有状态变更消息才触发 reload（新任务开始上传）
          // progress 和 completed 消息对于不在列表中的任务忽略，
          // 避免 60000 文件场景下不断触发全量刷新
          if (msg.type === "upload_start") {
            needsFullReload = true;
          }
          continue;
        }

        const task = { ...tasks[idx] };
        let taskChanged = false;

        switch (msg.type) {
          case "progress":
          case "upload_progress":
            if (msg.uploaded_chunks !== undefined && msg.uploaded_chunks !== task.uploaded_chunks) {
              task.uploaded_chunks = msg.uploaded_chunks;
              taskChanged = true;
            }
            if (msg.total_chunks !== undefined && msg.total_chunks !== task.total_chunks) {
              task.total_chunks = msg.total_chunks;
              taskChanged = true;
            }
            if (task.status !== "running") {
              task.status = "running";
              taskChanged = true;
            }
            break;
          case "completed":
          case "upload_complete":
            if (task.status !== "completed") {
              task.status = "completed";
              task.uploaded_chunks = task.total_chunks;
              taskChanged = true;
            }
            break;
          case "task_status":
            if (msg.status && task.status !== msg.status) {
              task.status = msg.status as TaskStatus;
              taskChanged = true;
            }
            break;
          case "task_cancelled":
            if (task.status !== "cancelled") {
              task.status = "cancelled";
              taskChanged = true;
            }
            break;
          case "upload_start":
            if (task.status !== "running") {
              task.status = "running";
              taskChanged = true;
            }
            if (msg.total_chunks !== undefined && msg.total_chunks !== task.total_chunks) {
              task.total_chunks = msg.total_chunks;
              taskChanged = true;
            }
            break;
          case "paused":
            if (task.status !== "paused") {
              task.status = "paused";
              taskChanged = true;
            }
            break;
          case "resumed":
            if (task.status !== "pending") {
              task.status = "pending";
              taskChanged = true;
            }
            break;
        }

        if (taskChanged) {
          tasks[idx] = task;
          changed = true;
        }
      }

      return changed ? { tasks } : state;
    });
  }

  if (needsFullReload) {
    // 使用较长的防抖间隔（1.5秒），避免 init 阶段高频 upload_queued 消息
    // 不断触发 loadTasks 导致 IPC 拥堵
    _debouncedLoadTasks(get, 1500);
  }
}

export const useTransferStore = create<TransferStore>((set, get) => ({
  tasks: [],
  config: null,
  isLoggedIn: false,
  username: null,
  wsConnected: false,
  wsClient: null,
  loginSuccessToast: false,

  loadTasks: async () => {
    if (_loadTasksInFlight) return; // 防止并发调用
    _loadTasksInFlight = true;
    try {
      const tasks = await invoke<TransferTask[]>("get_tasks");
      set({ tasks });
    } catch (err) {
      console.error("Failed to load tasks:", err);
    } finally {
      _loadTasksInFlight = false;
    }
  },

  cancelTask: async (uploadId: string) => {
    try {
      await invoke<boolean>("cancel_task", { uploadId });
      await get().loadTasks();
    } catch (err) {
      console.error("Failed to cancel task:", err);
    }
  },

  pauseTask: async (uploadId: string) => {
    try {
      await invoke<boolean>("pause_task", { uploadId });
      await get().loadTasks();
    } catch (err) {
      console.error("Failed to pause task:", err);
    }
  },

  resumeTask: async (uploadId: string) => {
    try {
      await invoke<boolean>("resume_task", { uploadId });
      await get().loadTasks();
    } catch (err) {
      console.error("Failed to resume task:", err);
    }
  },

  pauseAll: async () => {
    await invoke<void>("pause_all_tasks");
    await get().loadTasks();
  },

  resumeAll: async () => {
    await invoke<void>("resume_all_tasks");
    await get().loadTasks();
    // resume_task 是异步 fire-and-forget，状态变更有延迟，多次刷新兜底
    setTimeout(() => get().loadTasks(), 800);
    setTimeout(() => get().loadTasks(), 2000);
    setTimeout(() => get().loadTasks(), 4000);
  },

  cancelAll: async () => {
    await invoke<void>("cancel_all_tasks");
    await get().loadTasks();
  },

  retryAll: async () => {
    const { tasks } = get();
    // 重试失败的任务，同时恢复卡在等待中的孤立任务
    const targets = tasks.filter((t) => t.status === "failed" || t.status === "pending");
    // 分批调用，避免同时发起数千个 IPC 请求
    const BATCH_SIZE = 50;
    for (let i = 0; i < targets.length; i += BATCH_SIZE) {
      const batch = targets.slice(i, i + BATCH_SIZE);
      await Promise.allSettled(
        batch.map((t) => invoke<boolean>("resume_task", { uploadId: t.upload_id }))
      );
    }
    await get().loadTasks();
  },

  clearCompleted: async () => {
    try {
      await invoke<void>("clear_completed_tasks");
      await get().loadTasks();
    } catch (err) {
      console.error("Failed to clear completed tasks:", err);
    }
  },

  loadConfig: async () => {
    try {
      const config = await invoke<AppConfig>("get_config");
      set({ config });
    } catch (err) {
      console.error("Failed to load config:", err);
    }
  },

  checkAuth: async () => {
    try {
      const loggedIn = await invoke<boolean>("is_logged_in");
      const username = loggedIn ? await invoke<string | null>("get_current_username") : null;
      set({ isLoggedIn: loggedIn, username });
    } catch (err) {
      console.error("Failed to check auth:", err);
    }
  },

  openLoginPage: async () => {
    try {
      await invoke("open_login_page");
    } catch (err) {
      console.error("Failed to open login page:", err);
    }
  },

  logout: async () => {
    try {
      await invoke("logout");
      set({ isLoggedIn: false, username: null, tasks: [] });
      console.log("Logged out successfully");
    } catch (err) {
      console.error("Failed to logout:", err);
      // 即使 Rust 报错，前端也强制清除状态
      set({ isLoggedIn: false, username: null, tasks: [] });
    }
  },

  connectWs: () => {
    const { config, wsClient } = get();
    if (wsClient) return; // Already connected

    const port = config?.ws_port ?? 17892;
    let ws: WebSocket;
    try {
      ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    } catch (err) {
      console.error("Failed to create WebSocket:", err);
      setTimeout(() => {
        if (!get().wsClient) get().connectWs();
      }, 5000);
      return;
    }

    // Set client immediately so duplicate calls are blocked
    set({ wsClient: ws });

    ws.onopen = () => {
      console.log("WS connected to port", port);
      set({ wsConnected: true });
      // Load tasks once connected
      get().loadTasks();
    };

    ws.onmessage = (event) => {
      try {
        const msg: WsProgressMessage = JSON.parse(event.data);
        get()._updateTaskFromWs(msg);
      } catch {
        // ignore parse errors
      }
    };

    ws.onclose = () => {
      console.log("WS disconnected, reconnecting in 3s...");
      set({ wsConnected: false, wsClient: null });
      setTimeout(() => {
        if (!get().wsClient) get().connectWs();
      }, 3000);
    };

    ws.onerror = (err) => {
      console.error("WS error:", err);
    };
  },

  disconnectWs: () => {
    const { wsClient } = get();
    if (wsClient) {
      wsClient.close();
      set({ wsClient: null, wsConnected: false });
    }
  },

  _updateTaskFromWs: (msg: WsProgressMessage) => {
    // 将消息放入待处理队列，由 requestAnimationFrame 批量处理
    // 这样一帧内收到的多条消息只触发一次 React 重渲染
    _pendingWsMessages.push(msg);
    _scheduleWsFlush(get);
  },
}));

// Setup Tauri event listeners
export async function setupTauriListeners(store: ReturnType<typeof useTransferStore.getState>) {
  // Listen for upload:select-file events
  await listen("upload:select-file", () => {
    // Tasks will be updated via WS
    setTimeout(() => store.loadTasks(), 200);
  });

  // 收到 token 直接设置登录态，无需再 invoke keyring
  await listen<string>("auth:token-saved", () => {
    useTransferStore.setState({ isLoggedIn: true, loginSuccessToast: true });
    // 刷新用户名
    invoke<string | null>("get_current_username").then(username => {
      useTransferStore.setState({ username });
    });
    setTimeout(() => store.loadTasks(), 300);
    // 3秒后自动隐藏提示
    setTimeout(() => useTransferStore.setState({ loginSuccessToast: false }), 3000);
  });

  // 用户切换后任务列表已重载，前端同步刷新
  await listen("tasks:reloaded", () => {
    store.loadTasks();
  });

  // 用户不匹配，弹出提示
  await listen("upload:user-mismatch", () => {
    // 用原生 alert，简单直接
    alert("上传请求被拒绝：网页登录的用户与客户端登录的用户不一致，请先在客户端退出登录或使用相同账号登录网页。");
  });
}
