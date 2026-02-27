# Nodes example 2 — Dataflow groups (visual servoing)

This example illustrates a visual servoing system where two nodes form a feedback loop using **dataflow groups**.

A vision node detects an object and publishes its position. An arm controller subscribes to that position, moves the arm, and publishes the current arm position. The vision node subscribes to the arm position to refine its estimate using the wrist-mounted camera. The two nodes continuously feed each other's outputs.

The `dataflow` section in each node's `peppy.json5` declares that both nodes participate in the same `visual_servo_loop` group. This makes the bidirectional relationship explicit in the configuration.
