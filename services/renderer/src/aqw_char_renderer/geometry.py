"""Shared-canvas geometry and frame-manifest validation."""

from __future__ import annotations

import math
from collections.abc import Iterable, Sequence

Bounds = tuple[float, float, float, float]


def union_bounds(values: Iterable[Sequence[float]]) -> Bounds:
    bounds = [tuple(float(component) for component in value) for value in values]
    if not bounds:
        raise ValueError("at least one frame bound is required")
    if any(
        len(value) != 4
        or not all(math.isfinite(component) for component in value)
        or value[2] <= 0
        or value[3] <= 0
        for value in bounds
    ):
        raise ValueError("frame bounds must contain finite positive dimensions")
    left = min(value[0] for value in bounds)
    top = min(value[1] for value in bounds)
    right = max(value[0] + value[2] for value in bounds)
    bottom = max(value[1] + value[3] for value in bounds)
    return left, top, right - left, bottom - top


def shared_canvas(values: Iterable[Sequence[float]], *, max_size: int, padding: int) -> Bounds:
    if max_size < 1:
        raise ValueError("max_size must be positive")
    if padding < 0 or padding * 2 >= max_size:
        raise ValueError("padding must be nonnegative and less than half max_size")
    left, top, width, height = union_bounds(values)
    content_pixels = max(1, max_size - padding * 2)
    units_per_pixel = max(width, height) / content_pixels
    padding_units = padding * units_per_pixel
    return (
        left - padding_units,
        top - padding_units,
        width + padding_units * 2,
        height + padding_units * 2,
    )
