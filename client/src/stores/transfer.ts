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

  // Actions
  loadTasks: () => Promise<void>;
  cancelTask: (uploadId: string) => Promise<void>;
  pauseTask: (uploadId: string) => Promise<void>;
  resumeTask: (uploadId: string) => Promise<void>;
  loadConfig: () => Promise<void>;
  checkAuth: () => Promise<void>;
  connectWs: () => void;
  disconnectWs: () => void;
  openLoginPage: () => Promise<void>;
  logout: () => Promise<void>;

  // Internal
  _updateTaskFromWs: (msg: WsProgressMessage) => void;
}

export const useTransferStore = create<TransferStore>((set, get) => ({
  tasks: [],
  config: null,
  isLoggedIn: false,
  username: null,
  wsConnected: false,
  wsClient: null,

  loadTasks: async () => {
    try {
      const tasks = await invoke<TransferTask[]>("get_tasks");
      set({ tasks });
    } catch (err) {
      console.error("Failed to load tasks:", err);
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
        const msg: WsProgressMessage
      }, 3000);
    };

    ws.onerror = (err) => {
      console.error("WS error:", err);
    };

    set({ wsClient: ws });
  },

  disconnectWs: () => {
    const { wsClient } = get();
    if (wsClient) {
      wsClient.close();
      set({ wsClient: null, wsConnected: false });
    }
  },

  _updateTaskFromWs: (msg: WsProgressMessage) => {
    set((state) => {
      const tasks = [...state.tasks];
      const idx = tasks.findIndex((t) => t.upload_id === msg.upload_id);

      if (idx === -1) {
        // Unknown task, trigger reload
        setTimeout(() => get().loadTasks(), 100);
        return state;
      }

      const task = { ...tasks[idx] };

      switch (msg.type) {
        case "upload_progress":
          task.uploaded_chunks = msg.uploaded_chunks ?? task.uploaded_chunks;
          task.total_chunks = msg.total_chunks ?? task.total_chunks;
          task.status = "running";
          break;
        case "upload_complete":
          task.status = "completed";
          task.uploaded_chunks = task.total_chunks;
          break;
        case "task_status":
          if (msg.status) {
            task.status = msg.status as TaskStatus;
          }
          break;
        case "task_cancelled":
          task.status = "cancelled";
          break;
        case "upload_start":
          task.status = "running";
          task.total_chunks = msg.total_chunks ?? task.total_chunks;
          break;
      }

      tasks[idx] = task;
      return { tasks };
    });
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
    useTransferStore.setState({ isLoggedIn: true });
    // 刷新用户名
    invoke<string | null>("get_current_username").then(username => {
      useTransferStore.setState({ username });
    });
    setTimeout(() => store.loadTasks(), 300);
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
