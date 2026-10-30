import { expect, test } from "@playwright/test";
import type { BoxStats, StatsGroup, StatsSummary } from "../src/bindings";

const current = { model: "model-fast", accelerator: "gpu" };
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
  config: { model, accelerator: model === current.model ? "gpu" : "cpu" },
  is_current: model === current.model,
  runs: 10,
  failures,
  first_run: 1_793_260_800,
  last_run: 1_793_264_400,
  entry_ms: box(model === "model-fast" ? 100 : 50),
  exit_ms: box(median * 10_000),
  post_process_ms: model === "model-slow" ? box(300) : null,
  legacy_timing_runs: 0,
});

const summary: StatsSummary = {
  current,
  groups: [
    group("model-slow", "ru", 0.4, 0),
    group("model-fast", "en", 0.1, 2),
    {
      ...group("model-failed", null, 0, 10),
      entry_ms: null,
      exit_ms: null,
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
  await page.getByRole("tab", { name: "Entry", exact: true }).click();
  await expect(rows.first()).toContainText("model-slow");
  await expect(rows.first()).toContainText("50 ms");
  await page.getByRole("tab", { name: "Exit", exact: true }).click();
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
    "No timing samples yet",
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
        exit_ms: {
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
  await page.getByRole("tab", { name: "Entry", exact: true }).click();
  await expect(axis.locator("text").first()).toContainText("ms");
  expect(
    parseFloat((await axis.locator("text").first().textContent()) ?? ""),
  ).toBeLessThanOrEqual(25);
  expect(
    parseFloat((await axis.locator("text").last().textContent()) ?? ""),
  ).toBeGreaterThanOrEqual(100);
});

test("three latency tabs expose measured post-processing and explain unsplittable history", async ({
  page,
}, testInfo) => {
  await expect(page.getByRole("tab")).toHaveCount(3);
  const exit = page.getByRole("tab", { name: "Exit", exact: true });
  await exit.focus();
  await page.keyboard.press("ArrowRight");
  await expect(
    page.getByRole("tab", { name: "Post-processing", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  const rows = page.getByRole("listitem");
  await expect(rows.first()).toContainText("model-slow");
  await expect(rows.first()).toContainText("300 ms");
  await page.screenshot({
    path: testInfo.outputPath("statistics-post-processing.png"),
    fullPage: true,
  });
  await expect(
    rows.filter({ hasText: "model-fast" }).getByRole("img"),
  ).toHaveAccessibleName("No timing samples yet");
  await page.getByRole("button", { name: "All languages" }).click();
  await page.getByRole("button", { name: "English", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Zoom in", exact: true }),
  ).toBeDisabled();
  await expect(rows.first().locator("svg rect")).toHaveCount(0);
  await page.keyboard.press("Escape");
  await page.evaluate(() => {
    const state = (
      window as unknown as { statisticsFixture: { summary: StatsSummary } }
    ).statisticsFixture;
    state.summary.groups[1].legacy_timing_runs = 2;
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(page.getByText(/Older runs retain Entry/)).toBeVisible();
  await page.getByRole("tab", { name: "Entry", exact: true }).click();
  await expect(page.getByText(/Older runs retain Entry/)).toHaveCount(0);
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
  await page.getByRole("tab", { name: "Post-processing", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Fit data", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  await page.getByRole("tab", { name: "Exit", exact: true }).click();
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
  await page.getByRole("tab", { name: "Entry", exact: true }).click();
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
