"""Deterministic frame batching shared by the local and AWS coordinators."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any


@dataclass(frozen=True)
class FrameBatch:
    index: int
    frame_start: int
    frame_end: int

    @property
    def frame_count(self) -> int:
        return self.frame_end - self.frame_start + 1

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def partition_frames(frame_count: int, batch_size: int) -> list[FrameBatch]:
    if frame_count < 1:
        raise ValueError("frame_count must be positive")
    if batch_size < 1:
        raise ValueError("batch_size must be positive")
    return [
        FrameBatch(
            index=index,
            frame_start=start,
            frame_end=min(frame_count, start + batch_size - 1),
        )
        for index, start in enumerate(range(1, frame_count + 1, batch_size))
    ]
