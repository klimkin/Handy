import React, { useId } from "react";
import type { BoxStats } from "@/bindings";
import { formatAxisTick, scaleX, type PlotAxis } from "./statsView";

export const BOX_PLOT_PAD = 8;
export const BOX_PLOT_HEIGHT = 32;

interface BoxPlotProps {
  box: BoxStats | null;
  axis: PlotAxis;
  width: number;
  highlighted: boolean;
  /** Spoken by tooltips and screen readers: the figures in words. */
  summary: string;
}

/** Tick labels for the scale every box plot below it is drawn on. */
export const BoxPlotAxis: React.FC<{
  axis: PlotAxis;
  width: number;
}> = ({ axis, width }) => {
  const budget = Math.min(
    axis.ticks.length,
    Math.max(1, Math.floor((width - BOX_PLOT_PAD * 2) / 80) + 1),
  );
  let labels: number[] = [];
  for (let count = budget; count > 0; count--) {
    const candidates =
      count === 1
        ? [axis.ticks[Math.floor((axis.ticks.length - 1) / 2)]]
        : Array.from(
            { length: count },
            (_, index) =>
              axis.ticks[
                Math.round((index * (axis.ticks.length - 1)) / (count - 1))
              ],
          );
    let right = Number.NEGATIVE_INFINITY;
    const fits = candidates.every((tick, index) => {
      const x = scaleX(tick, axis, width, BOX_PLOT_PAD);
      const estimatedWidth = formatAxisTick(tick, axis.max).length * 7;
      const left =
        x -
        (count === 1
          ? estimatedWidth / 2
          : index === 0
            ? 0
            : index === count - 1
              ? estimatedWidth
              : estimatedWidth / 2);
      const end = left + estimatedWidth;
      const separate = left >= 0 && end <= width && left >= right + 8;
      right = end;
      return separate;
    });
    if (fits) {
      labels = candidates;
      break;
    }
  }
  return (
    <svg
      data-plot-chart
      width={width}
      height={24}
      aria-hidden="true"
      className="block cursor-grab active:cursor-grabbing touch-none select-none"
    >
      {labels.map((tick, index) => (
        <text
          key={tick}
          x={scaleX(tick, axis, width, BOX_PLOT_PAD)}
          y={12}
          textAnchor={
            labels.length === 1
              ? "middle"
              : index === 0
                ? "start"
                : index === labels.length - 1
                  ? "end"
                  : "middle"
          }
          className="fill-ink-3 text-[10px]"
        >
          {formatAxisTick(tick, axis.max)}
        </text>
      ))}
    </svg>
  );
};

/**
 * An extended box plot: whiskers from the smallest to the largest value, a
 * box from the first to the third quartile, a line at the median and a
 * diamond at the mean.
 */
export const BoxPlot: React.FC<BoxPlotProps> = ({
  box,
  axis,
  width,
  highlighted,
  summary,
}) => {
  const x = (value: number) => scaleX(value, axis, width, BOX_PLOT_PAD);
  const mid = BOX_PLOT_HEIGHT / 2;
  const clip = useId();

  return (
    <svg
      width={width}
      height={BOX_PLOT_HEIGHT}
      role="img"
      aria-label={summary}
      data-plot-chart
      className="block cursor-grab active:cursor-grabbing touch-none select-none"
    >
      <title>{summary}</title>
      <defs>
        <clipPath id={clip}>
          <path
            d={`M${BOX_PLOT_PAD},0H${Math.max(BOX_PLOT_PAD + 1, width - BOX_PLOT_PAD)}V${BOX_PLOT_HEIGHT}H${BOX_PLOT_PAD}Z`}
          />
        </clipPath>
      </defs>
      {axis.ticks.map((tick) => (
        <line
          key={tick}
          x1={x(tick)}
          x2={x(tick)}
          y1={0}
          y2={BOX_PLOT_HEIGHT}
          className="stroke-line"
        />
      ))}
      {box && (
        <g
          className={highlighted ? "stroke-plum" : "stroke-logo-primary"}
          strokeWidth={1.5}
          clipPath={`url(#${clip})`}
        >
          <line x1={x(box.min)} x2={x(box.max)} y1={mid} y2={mid} />
          <line x1={x(box.min)} x2={x(box.min)} y1={mid - 6} y2={mid + 6} />
          <line x1={x(box.max)} x2={x(box.max)} y1={mid - 6} y2={mid + 6} />
          <rect
            x={x(box.q1)}
            y={mid - 9}
            width={Math.max(x(box.q3) - x(box.q1), 2)}
            height={18}
            className={highlighted ? "fill-plum/30" : "fill-logo-primary/30"}
          />
          <line
            x1={x(box.median)}
            x2={x(box.median)}
            y1={mid - 9}
            y2={mid + 9}
            strokeWidth={3}
          />
          <polygon
            points={`${x(box.mean)},${mid - 4} ${x(box.mean) + 4},${mid} ${x(box.mean)},${mid + 4} ${x(box.mean) - 4},${mid}`}
            className="fill-background stroke-text"
            strokeWidth={1.25}
          />
        </g>
      )}
      {box && box.min < axis.min && (
        <path
          d={`M7,${mid - 4}L2,${mid}L7,${mid + 4}`}
          className="fill-none stroke-ink-3"
          strokeWidth={1.5}
        />
      )}
      {box && box.max > axis.max && (
        <path
          d={`M${width - 7},${mid - 4}L${width - 2},${mid}L${width - 7},${mid + 4}`}
          className="fill-none stroke-ink-3"
          strokeWidth={1.5}
        />
      )}
    </svg>
  );
};
