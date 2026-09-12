/* @refresh reload */
import { render } from "@solidjs/web";
import { attachConsole } from "@tauri-apps/plugin-log";
import App from "./App";

// forwards Rust logs (tauri-plugin-log Webview target) to devtools console
attachConsole().catch((error) => console.error("log attach failed", error));

render(() => <App />, document.getElementById("root") as HTMLElement);
