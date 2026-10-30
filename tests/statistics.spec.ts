import { expect, test } from "@playwright/test";
import type { BoxStats, StatsGroup, StatsSummary } from "../src/bindings";

const current = {
  model: "model-fast",
  accelerator: "gpu",
  post_process: "false",
};
const box = (median: number): BoxStats => ({
  min: median / 2,
  q1: median * 0.75,
  median,
  q3: median * 1.25,
  max: median * 2,
  mean: median * 1.1,
});
const group = (
  model: string,
  language: string | null,
  median: number,
  failures: number,
): StatsGroup => ({
  model,
  language,
  variant: 1,
  config: {
    model,
    accelerator: model === current.model ? "gpu" : "cpu",
    post_process: model === "model-slow" ? "true" : "false",
    ...(model === "model-slow"
      ? {
          post_process_provider: "local",
          post_process_model: "llm",
          post_process_prompt_sha256: "abc123",
        }
      : {}),
  },
  is_current: model === current.model,
  runs: 10,
  failures,
  first_run: 1_793_260_800,
  last_run: 1_793_264_400,
  capture_gap_ms: box(model === "model-fast" ? 100 : 50),
  output_gap_ms: box(median * 10_000),
  capture_missing: 0,
  output_missing: failures,
  cancellations: 0,
  capture_unavailable_reasons: {},
  output_unavailable_reasons: {},
});

const summary: StatsSummary = {
  current,
  legacy_runs: 3,
  trace_path: "/data/telemetry/dictation-events.jsonl",
  trace_error: null,
  pending_exports: 0,
  groups: [
    group("model-slow", "ru", 0.4, 0),
    group("model-fast", "en", 0.1, 2),
    {
      ...group("model-failed", null, 0, 10),
      capture_gap_ms: null,
      output_gap_ms: null,
      capture_missing: 10,
      capture_unavailable_reasons: { sample_timestamps_unavailable: 10 },
    },
  ],
};

test.beforeEach(async ({ page }) => {
  await page.addInitScript((initial) => {
    Object.assign(window, {
      statisticsFixture: {
        summary: initial,
        reject: false,
        cleared: 0,
      },
    });
  }, summary);
  await page.route("**/src/main.tsx", (route) =>
    route.fulfill({
      contentType: "application/javascript",
      body: `
        import React from "/node_modules/.vite/deps/react.js";
        import ReactDOM from "/node_modules/.vite/deps/react-dom_client.js";
        import { commands, events } from "/src/bindings.ts";
        import { StatisticsSettings } from "/src/components/settings/statistics/StatisticsSettings.tsx";
        import i18n from "/src/i18n/index.ts";
        import "/src/App.css";
        commands.getAppSettings = async () => ({ status: "ok", data: { app_language: "en" } });
        commands.getStatsSummary = async () => {
          await new Promise(resolve => setTimeout(resolve, 50));
          if (window.statisticsFixture.reject) throw new Error("storage unavailable");
          return { status: "ok", data: structuredClone(window.statisticsFixture.summary) };
        };
        commands.clearStats = async () => {
          window.statisticsFixture.cleared++;
          window.statisticsFixture.summary.groups = [];
          return { status: "ok", data: null };
        };
        events.transcriptionCompleteEvent.listen = async () => () => {};
        await i18n.changeLanguage("en");
        ReactDOM.createRoot(document.getElementById("root")).render(
          React.createElement(StatisticsSettings)
        );
      `,
    }),
  );
  await page.goto("/");
});

test("plots arriving after load have a shared responsive scale and show configuration differences", async ({
  page,
}, testInfo) => {
  const rows = page.getByRole("listitem");
  await expect(rows).toHaveCount(3);
  await expect(rows.first()).toContainText("model-fast");
  const chart = rows.first().getByRole("img");
  await expect
    .poll(async () => Number(await chart.getAttribute("width")))
    .toBeGreaterThan(100);
  const before = Number(await chart.getAttribute("width"));
  await page.setViewportSize({ width: 600, height: 900 });
  await expect
    .poll(async () => Number(await chart.getAttribute("width")))
    .toBeLessThan(before);
  await expect(rows.first()).toContainText("1.0 s");

  const slow = rows.filter({ hasText: "model-slow" });
  await slow.getByRole("button", { name: /model-slow/ }).click();
  await expect(
    slow.getByRole("button", { name: /model-slow/ }),
  ).toHaveAttribute("aria-pressed", "true");
  await expect(rows.first()).toHaveClass(/opacity-50/);
  await slow.getByRole("button", { name: /model-slow/ }).dblclick();
  await expect(slow.getByRole("table")).toBeVisible();
  await expect(
    slow.getByRole("row", { name: "Accelerator cpu gpu" }),
  ).toBeVisible();
  await page.getByRole("tab", { name: "Capture gap", exact: true }).click();
  await expect(rows.first()).toContainText("model-slow");
  await expect(rows.first()).toContainText("50 ms");
  await page.getByRole("tab", { name: "Output gap", exact: true }).click();
  await expect(rows.first()).toContainText("1.0 s");
  await page.getByRole("button", { name: "Fewest failures first" }).click();
  await expect(rows.first()).toContainText("model-slow");
  await page.screenshot({
    path: testInfo.outputPath("statistics.png"),
    fullPage: true,
  });
});

test("language filtering and clearing reset the page without hiding new data", async ({
  page,
}) => {
  const rows = page.getByRole("listitem");
  await expect(rows).toHaveCount(3);
  await page.getByRole("button", { name: "All languages" }).click();
  await page.getByRole("button", { name: "Russian", exact: true }).click();
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("model-slow");
  await page.getByRole("button", { name: "Zoom in", exact: true }).click();
  await page.getByRole("button", { name: "Clear statistics" }).click();
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(rows).toHaveCount(1);
  await page.getByRole("button", { name: "Clear statistics" }).click();
  await page.getByRole("button", { name: "Delete all runs" }).click();
  await expect(rows).toHaveCount(0);
  await page.evaluate((initial) => {
    const state = (
      window as unknown as {
        statisticsFixture: { summary: StatsSummary };
      }
    ).statisticsFixture;
    state.summary = { ...initial, groups: [initial.groups[1]] };
  }, summary);
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("model-fast");
  await expect(
    page.getByRole("button", { name: "Fit data", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
});

test("failed groups have no invented timing and command errors appear on the page", async ({
  page,
}) => {
  const failed = page.getByRole("listitem").filter({ hasText: "model-failed" });
  await expect(failed).toContainText("100%");
  await expect(failed.getByRole("img")).toHaveAccessibleName(
    /No timing samples yet/,
  );
  await expect(failed.locator("svg rect")).toHaveCount(0);
  await page.evaluate(() => {
    const state = (
      window as unknown as { statisticsFixture: { summary: StatsSummary } }
    ).statisticsFixture;
    state.summary.groups = state.summary.groups.filter(
      (group) => group.model === "model-failed",
    );
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(page.getByRole("listitem")).toHaveCount(1);
  await expect(
    page.getByRole("button", { name: "Fit data", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "From zero", exact: true }),
  ).toBeDisabled();
  await page.evaluate(() => {
    (
      window as unknown as { statisticsFixture: { reject: boolean } }
    ).statisticsFixture.reject = true;
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(
    page.getByText(/Couldn't load statistics:.*storage unavailable/),
  ).toBeVisible();
});

test("fit data expands nearby points without clipping and keeps narrow-axis labels separate", async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 700, height: 900 });
  await page.evaluate(() => {
    const state = (
      window as unknown as { statisticsFixture: { summary: StatsSummary } }
    ).statisticsFixture;
    state.summary.groups = state.summary.groups.slice(0, 2).map((group, i) => {
      const value = i === 0 ? 1600 : 1200;
      return {
        ...group,
        runs: 2,
        failures: 1,
        output_missing: 1,
        output_gap_ms: {
          min: value,
          q1: value,
          median: value,
          q3: value,
          max: value,
          mean: value,
        },
      };
    });
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  const rows = page.getByRole("listitem");
  await expect(rows).toHaveCount(2);
  const fit = page.getByRole("button", { name: "Fit data", exact: true });
  await expect(fit).toHaveAttribute("aria-pressed", "true");
  const axis = page.locator("svg").filter({ has: page.locator("text") });
  await expect(axis.locator("text").first()).not.toHaveText("0 s");

  const separation = async () => {
    const positions = await rows
      .locator('line[stroke-width="3"]')
      .evaluateAll((lines) =>
        lines.map((line) => Number(line.getAttribute("x1"))),
      );
    return Math.abs(positions[1] - positions[0]);
  };
  const focusedGap = await separation();
  await expect(rows.first().locator("svg rect")).toHaveAttribute("width", "2");
  await page.screenshot({
    path: testInfo.outputPath("statistics-fit.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: "From zero", exact: true }).click();
  await expect(axis.locator("text").first()).toHaveText("0 s");
  expect(focusedGap).toBeGreaterThan((await separation()) * 1.5);
  await fit.click();
  await page.setViewportSize({ width: 600, height: 900 });
  await expect
    .poll(async () => {
      const bounds = await axis.locator("text").evaluateAll((texts) =>
        texts.map((text) => {
          const rect = text.getBoundingClientRect();
          return { left: rect.left, right: rect.right };
        }),
      );
      return bounds.every(
        (rect, i) => i === 0 || rect.left >= bounds[i - 1].right + 4,
      );
    })
    .toBe(true);
  await page.screenshot({
    path: testInfo.outputPath("statistics-narrow.png"),
    fullPage: true,
  });
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(600);
  await page.getByRole("button", { name: "All languages" }).click();
  await page.getByRole("button", { name: "Russian", exact: true }).click();
  await expect(rows).toHaveCount(1);
  const singlePosition = await rows
    .first()
    .getByRole("img")
    .evaluate((plot) => {
      const line = plot.querySelector('line[stroke-width="3"]');
      return (
        Number(line?.getAttribute("x1")) / Number(plot.getAttribute("width"))
      );
    });
  expect(singlePosition).toBeGreaterThan(0.1);
  expect(singlePosition).toBeLessThan(0.9);
  await page.getByRole("tab", { name: "Capture gap", exact: true }).click();
  await expect(axis.locator("text").first()).toContainText("ms");
  expect(
    parseFloat((await axis.locator("text").first().textContent()) ?? ""),
  ).toBeLessThanOrEqual(25);
  expect(
    parseFloat((await axis.locator("text").last().textContent()) ?? ""),
  ).toBeGreaterThanOrEqual(100);
});

test("two gap tabs explain evidence, LLM configurations, missing samples and cancellations", async ({
  page,
}, testInfo) => {
  await expect(page.getByRole("tab")).toHaveCount(2);
  const output = page.getByRole("tab", { name: "Output gap", exact: true });
  await output.focus();
  await page.keyboard.press("ArrowRight");
  await expect(
    page.getByRole("tab", { name: "Capture gap", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect(
    page.getByText(/Sample timestamps or continuous coverage unavailable/),
  ).toBeVisible();
  const failed = page.getByRole("listitem").filter({ hasText: "model-failed" });
  await expect(failed).toContainText(
    "0 measured · 10 unavailable · 0 cancelled",
  );
  await expect(failed.getByRole("img")).toHaveAccessibleName(
    /No timing samples yet.*10 unavailable/,
  );
  await page.screenshot({
    path: testInfo.outputPath("statistics-capture-unavailable.png"),
    fullPage: true,
  });
  await output.click();
  await expect(page.getByText(/including LLM post-processing/)).toBeVisible();
  const slow = page.getByRole("listitem").filter({ hasText: "model-slow" });
  await slow.getByRole("button", { name: /model-slow/ }).dblclick();
  await expect(
    slow.getByRole("row", { name: "Post-processing On Off" }),
  ).toBeVisible();
  await expect(
    slow.getByRole("row", { name: /Prompt fingerprint abc123/ }),
  ).toBeVisible();
  await page.evaluate(() => {
    const state = (
      window as unknown as { statisticsFixture: { summary: StatsSummary } }
    ).statisticsFixture;
    state.summary.groups = [
      {
        ...state.summary.groups[2],
        runs: 15,
        failures: 2,
        cancellations: 5,
        capture_missing: 10,
        output_missing: 10,
      },
    ];
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  const row = page.getByRole("listitem");
  await expect(row).toHaveCount(1);
  await expect(row).toContainText("0 measured · 10 unavailable · 5 cancelled");
  await expect(row).toContainText("20%");
  await expect(row).toContainText("2/10");
  await expect(page.getByText(/3 older runs are preserved/)).toBeVisible();
  await page.getByRole("tab", { name: "Capture gap", exact: true }).click();
  await page.screenshot({
    path: testInfo.outputPath("statistics-capture-unavailable.png"),
    fullPage: true,
  });
});

test("legacy-only data keeps trace path and export status visible", async ({
  page,
  context,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await expect(page.getByText(summary.trace_path)).toBeVisible();
  await page.getByRole("button", { name: "Copy event file path" }).click();
  await expect(
    page.getByRole("button", { name: "Copy event file path" }),
  ).toHaveText("Copied");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(
    summary.trace_path,
  );
  await page.evaluate(() => {
    const state = (
      window as unknown as { statisticsFixture: { summary: StatsSummary } }
    ).statisticsFixture;
    state.summary.groups = [];
    state.summary.pending_exports = 2;
    state.summary.trace_error = "disk full";
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(page.getByText(/3 older runs are preserved/)).toBeVisible();
  await expect(page.getByRole("status")).toHaveText(
    "Event file exports pending: 2",
  );
  await expect(page.getByRole("alert")).toHaveText(
    "Event file export failed: disk full",
  );
  await expect(page.getByText(summary.trace_path)).toBeVisible();
});

test("same-model LLM variants remain distinct and selectable", async ({
  page,
}) => {
  await page.evaluate(() => {
    const state = (
      window as unknown as { statisticsFixture: { summary: StatsSummary } }
    ).statisticsFixture;
    const plain = state.summary.groups[1];
    state.summary.groups = [
      plain,
      {
        ...plain,
        variant: 2,
        is_current: false,
        config: {
          ...plain.config,
          post_process: "true",
          post_process_provider: "local",
          post_process_model: "llm",
          post_process_prompt_sha256: "abc123",
        },
      },
    ];
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  const rows = page.getByRole("listitem");
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(1)).toContainText("Config #2");
  await rows
    .nth(1)
    .getByRole("button", { name: /model-fast/ })
    .dblclick();
  await expect(rows.first()).toHaveClass(/opacity-50/);
  await expect(rows.nth(1).getByRole("table")).toBeVisible();
  await expect(
    rows.nth(1).getByRole("row", { name: "Post-processing On Off" }),
  ).toBeVisible();
  await page.getByRole("tab", { name: "Capture gap", exact: true }).click();
  await expect(
    rows.nth(1).getByRole("button", { name: /model-fast/ }),
  ).toHaveAttribute("aria-pressed", "true");
});

test("copy failures remain visible and can be retried", async ({ page }) => {
  await page.evaluate(() => {
    Object.defineProperty(navigator.clipboard, "writeText", {
      configurable: true,
      value: async () => {
        throw new Error("clipboard denied");
      },
    });
  });
  await page.getByRole("button", { name: "Copy event file path" }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "Couldn't copy path: Error: clipboard denied",
  );
  await page.evaluate(() => {
    Object.defineProperty(navigator.clipboard, "writeText", {
      configurable: true,
      value: async () => {},
    });
  });
  await page.getByRole("button", { name: "Copy event file path" }).click();
  await expect(
    page.getByRole("button", { name: "Copy event file path" }),
  ).toHaveText("Copied");
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("wheel zoom stays anchored and shared, clips real geometry, pans without selecting, and resets", async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 900, height: 900 });
  const rows = page.getByRole("listitem");
  await expect(rows).toHaveCount(3);
  const plot = rows.first().getByRole("img");
  const median = plot.locator('line[stroke-width="3"]');
  const position = async () => Number(await median.getAttribute("x1"));
  const firstPosition = await position();
  const rect = await plot.boundingBox();
  if (!rect) throw new Error("Missing rendered plot");
  const pointer = Math.round(rect.x + rect.width / 2) - rect.x;
  const valueAtPointer = async () => {
    const endpoints = await plot
      .locator("g > line")
      .first()
      .evaluate((line) => ({
        min: Number(line.getAttribute("x1")),
        max: Number(line.getAttribute("x2")),
      }));
    return (
      500 + ((pointer - endpoints.min) / (endpoints.max - endpoints.min)) * 1500
    );
  };
  const beforeValue = await valueAtPointer();
  await page.mouse.move(rect.x + pointer, rect.y + rect.height / 2);
  await page.mouse.wheel(0, -300);
  await expect.poll(position).toBeLessThan(firstPosition);
  expect(await valueAtPointer()).toBeCloseTo(beforeValue, 6);
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
  await expect(plot).toHaveAccessibleName(/outside the visible range/);
  expect(
    Number(await plot.locator("g > line").first().getAttribute("x1")),
  ).toBeLessThan(8);
  await expect(plot.locator("g[clip-path]")).toHaveCount(1);
  const directions = await rows.getByRole("img").evaluateAll((plots) =>
    plots.slice(0, 2).map((svg) => {
      const line = svg.querySelector("g > line");
      return (
        Number(line?.getAttribute("x2")) - Number(line?.getAttribute("x1"))
      );
    }),
  );
  expect(directions[1] / directions[0]).toBeCloseTo(4, 6);
  await page.screenshot({
    path: testInfo.outputPath("statistics-wheel-zoom.png"),
    fullPage: true,
  });

  const beforePan = await position();
  await page.mouse.down();
  await page.mouse.move(rect.x + pointer + 45, rect.y + rect.height / 2, {
    steps: 6,
  });
  await page.mouse.up();
  await expect.poll(position).toBeGreaterThan(beforePan);
  await expect(
    rows.first().getByRole("button", { name: /model-fast/ }),
  ).toHaveAttribute("aria-pressed", "false");
  await expect(rows.first().getByRole("table")).toHaveCount(0);
  await page.getByRole("button", { name: "Fit data", exact: true }).click();
  await expect.poll(position).toBeCloseTo(firstPosition, 6);
  await expect(plot).not.toHaveAccessibleName(/outside the visible range/);
  await page.getByRole("button", { name: "Zoom in", exact: true }).click();
  await expect.poll(position).toBeLessThan(firstPosition);
  await page.getByRole("button", { name: "Zoom out", exact: true }).click();
  await expect.poll(position).toBeCloseTo(firstPosition, 6);

  await page.getByRole("button", { name: "Zoom in", exact: true }).click();
  await page.getByRole("tab", { name: "Capture gap", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Fit data", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  await page.getByRole("tab", { name: "Output gap", exact: true }).click();
  await expect.poll(position).toBeCloseTo(firstPosition, 6);

  const slow = rows.filter({ hasText: "model-slow" });
  await slow.getByRole("img").click();
  await expect(
    slow.getByRole("button", { name: /model-slow/ }),
  ).toHaveAttribute("aria-pressed", "true");
  await slow.getByRole("img").dblclick();
  await expect(slow.getByRole("table")).toBeVisible();

  await page.evaluate(() => {
    document.body.style.minHeight = "2000px";
  });
  await rows
    .first()
    .getByRole("button", { name: /model-fast/ })
    .hover();
  await page.mouse.wheel(0, 400);
  await expect
    .poll(() => page.evaluate(() => window.scrollY))
    .toBeGreaterThan(0);
  expect(await position()).toBeCloseTo(firstPosition, 6);
});

test("deep zoom keeps precise tick labels separate and stops at a finite limit", async ({
  page,
}) => {
  await page.setViewportSize({ width: 600, height: 900 });
  await page.getByRole("tab", { name: "Capture gap", exact: true }).click();
  const zoomIn = page.getByRole("button", { name: "Zoom in", exact: true });
  for (let step = 0; step < 12 && !(await zoomIn.isDisabled()); step++)
    await zoomIn.click();
  await expect(zoomIn).toBeDisabled();
  const ticks = page.locator("svg text");
  expect(await ticks.count()).toBeGreaterThan(0);
  const bounds = await ticks.evaluateAll((texts) =>
    texts.map((text) => {
      const rect = text.getBoundingClientRect();
      return { left: rect.left, right: rect.right };
    }),
  );
  expect(
    bounds.every(
      (rect, index) => index === 0 || rect.left >= bounds[index - 1].right + 4,
    ),
  ).toBe(true);
  await page.getByRole("button", { name: "Fit data", exact: true }).click();
  await expect(zoomIn).toBeEnabled();
});
