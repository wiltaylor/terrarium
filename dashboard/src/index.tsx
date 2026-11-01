import { render } from "solid-js/web";
import { App } from "./app/App";
import "./styles/global.css";
// The DB tab and the wiki `db` blocks (features/db, components/DbEmbed).
import "./styles/db.css";

render(() => <App />, document.getElementById("root")!);
