import { useEffect } from "react";
import { TransferQueue } from "./pages/TransferQueue";
import { setupTauriListeners, useTransferStore } from "./stores/transfer";

function App() {
  const store = useTransferStore.getState();

  useEffect(() => {
    // Load initial config
    store.loadConfig().then(() => {
      // Connect WS after config is loaded
      store.connectWs();
    });

    // Setup Tauri event listeners
    setupTauriListeners(store);

    return () => {
      store.disconnectWs();
    };
  }, []);

  return <TransferQueue />;
}

export default App;
