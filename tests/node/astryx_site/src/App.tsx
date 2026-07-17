import { Button } from "@astryxdesign/core/Button";
import { Text } from "@astryxdesign/core/Text";
import { live_reload_color } from "colors";

export default function App() {
  return (
    <div style={{ padding: 24 }}>
      <Text type="display-2">Hello World</Text>
      <Button label="Click me" variant="primary" />
      <Text style={{ color: live_reload_color() }}>Live reload color from dep</Text>
    </div>
  );
}
