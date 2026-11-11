/** Mount trusted controls only after the Markdown has passed the sanitizer. */
export function mountCodeCopy(root: ParentNode): void {
  root.querySelectorAll("pre > code").forEach((code) => {
    const pre = code.parentElement!;
    const text = code.textContent ?? "";
    const wrapper = document.createElement("div");
    wrapper.className = "code-block";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "code-copy";
    button.textContent = "Copy code";
    button.setAttribute("aria-live", "polite");
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(text);
        button.textContent = "Copied";
      } catch {
        button.textContent = "Copy failed. Try again";
      }
    });
    pre.replaceWith(wrapper);
    wrapper.append(button, pre);
  });
}
