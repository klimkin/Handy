import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent, PointerEvent } from "react";
import type { BoxStats } from "@/bindings";
import { BOX_PLOT_PAD } from "./BoxPlot";
import {
  axisForDomain,
  fitAxis,
  niceAxis,
  panDomain,
  zoomDomain,
  type PlotDomain,
} from "./statsView";

type Drag = {
  id: number;
  x: number;
  domain: PlotDomain;
  svg: SVGSVGElement;
  moved: boolean;
};

function plotAt(target: EventTarget | null): SVGSVGElement | null {
  return target instanceof Element
    ? target.closest<SVGSVGElement>("svg[data-plot-chart]")
    : null;
}

export function usePlotViewport(boxes: BoxStats[], context: string) {
  const [container, ref] = useState<HTMLDivElement | null>(null);
  const [custom, setCustom] = useState<{
    context: string;
    domain: PlotDomain;
  } | null>(null);
  const drag = useRef<Drag | null>(null);
  const suppressClick = useRef(false);
  const fitted = useMemo(() => fitAxis(boxes), [boxes]);
  const bounds = useMemo(() => niceAxis(0, fitted.max), [fitted]);
  const minSpan = (fitted.max - fitted.min) / 1024;
  const enabled = boxes.length > 0;
  const active = custom?.context === context ? custom : null;
  const domain = active ? panDomain(active.domain, bounds, 0) : fitted;
  const axis = axisForDomain(domain);

  useEffect(() => {
    setCustom(null);
    drag.current = null;
    suppressClick.current = false;
  }, [context]);

  const transform = useCallback(
    (update: (domain: PlotDomain) => PlotDomain) => {
      if (!enabled) return;
      setCustom((current) => ({
        context,
        domain: update(current?.context === context ? current.domain : fitted),
      }));
    },
    [context, enabled, fitted],
  );

  const zoom = useCallback(
    (factor: number, anchor = 0.5) => {
      transform((current) =>
        zoomDomain(current, bounds, minSpan, factor, anchor),
      );
    },
    [bounds, minSpan, transform],
  );

  useEffect(() => {
    if (!container || !enabled) return;
    const wheel = (event: WheelEvent) => {
      const svg = plotAt(event.target);
      if (!svg || event.deltaY === 0) return;
      event.preventDefault();
      const rect = svg.getBoundingClientRect();
      const unit =
        event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? rect.height : 1;
      const anchor =
        (event.clientX - rect.left - BOX_PLOT_PAD) /
        Math.max(1, rect.width - 2 * BOX_PLOT_PAD);
      zoom(
        Math.exp(Math.max(-1, Math.min(1, event.deltaY * unit * 0.002))),
        anchor,
      );
    };
    container.addEventListener("wheel", wheel, { passive: false });
    return () => container.removeEventListener("wheel", wheel);
  }, [container, enabled, zoom]);

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    suppressClick.current = false;
    const svg = plotAt(event.target);
    if (!enabled || event.button !== 0 || !svg) return;
    svg.setPointerCapture(event.pointerId);
    event.preventDefault();
    drag.current = {
      id: event.pointerId,
      x: event.clientX,
      domain,
      svg,
      moved: false,
    };
  };
  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const gesture = drag.current;
    if (!gesture || gesture.id !== event.pointerId) return;
    const dx = event.clientX - gesture.x;
    if (Math.abs(dx) >= 4) gesture.moved = true;
    if (!gesture.moved) return;
    const width = Math.max(
      1,
      gesture.svg.getBoundingClientRect().width - 2 * BOX_PLOT_PAD,
    );
    const delta = (-dx / width) * (gesture.domain.max - gesture.domain.min);
    transform(() => panDomain(gesture.domain, bounds, delta));
  };
  const endDrag = (event: PointerEvent<HTMLDivElement>) => {
    const gesture = drag.current;
    if (!gesture || gesture.id !== event.pointerId) return;
    suppressClick.current = gesture.moved;
    if (gesture.svg.hasPointerCapture(event.pointerId))
      gesture.svg.releasePointerCapture(event.pointerId);
    drag.current = null;
  };
  const onClickCapture = (event: MouseEvent<HTMLDivElement>) => {
    if (suppressClick.current && event.detail > 0) {
      event.preventDefault();
      event.stopPropagation();
    }
  };

  return {
    axis,
    ref,
    pointerHandlers: {
      onPointerDown,
      onPointerMove,
      onPointerUp: endDrag,
      onPointerCancel: endDrag,
      onClickCapture,
      onDoubleClickCapture: onClickCapture,
    },
    isFit: active === null,
    isZero:
      active !== null && domain.min === bounds.min && domain.max === bounds.max,
    canZoomIn: enabled && domain.max - domain.min > minSpan * (1 + 1e-9),
    canZoomOut:
      enabled &&
      domain.max - domain.min < (bounds.max - bounds.min) * (1 - 1e-9),
    zoomIn: () => zoom(0.5),
    zoomOut: () => zoom(2),
    reset: () => setCustom(null),
    fromZero: () => setCustom({ context, domain: bounds }),
  };
}
