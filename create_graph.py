import matplotlib.pyplot as plt
import numpy as np

# Graph 1: Per-Request Blocking Failure Cliff
concurrent = [10, 15, 20, 25, 30, 40, 50]
failure = [0, 0, 5.5, 16.2, 37.3, 74.2, 93.8]

plt.figure(figsize=(10, 6))
plt.plot(concurrent, failure, 'ro-', linewidth=2, markersize=8, color='#e74c3c')
plt.xlabel('Concurrent Requests', fontsize=12, fontweight='bold')
plt.ylabel('Failure %', fontsize=12, fontweight='bold')
plt.title('Per-Request Blocking: Failure Rate vs Concurrent Load', fontsize=14, fontweight='bold')
plt.grid(True, alpha=0.3)
plt.ylim(0, 100)

# Add annotation about the cliff
plt.annotate('Same blocking code.\nDifferent traffic level.',
             xy=(25, 50), xytext=(35, 65),
             arrowprops=dict(facecolor='black', shrink=0.05, width=1.5, headwidth=8),
             fontsize=11, bbox=dict(boxstyle='round,pad=0.5', facecolor='yellow', alpha=0.3))

plt.tight_layout()
plt.savefig('docs/blocking_failure_cliff.png', dpi=300, bbox_inches='tight')
print("Graph 1 saved as docs/blocking_failure_cliff.png")

# Graph 2: Benchmark Results - Scheduling Overhead Cliff
blockers = [0, 1, 2, 3, 4]
p50_latency = [1230, 1253, 1341, 1310, 1300]
p99_latency = [1387, 1490, 1959, 1564, 140415]

plt.figure(figsize=(10, 6))
plt.plot(blockers, p50_latency, 'go-', linewidth=2, markersize=8, label='p50 (median)', color='#27ae60')
plt.plot(blockers, p99_latency, 'ro-', linewidth=2, markersize=8, label='p99 (99th percentile)', color='#e74c3c')
plt.xlabel('Number of Blocking Tasks', fontsize=12, fontweight='bold')
plt.ylabel('Scheduling Overhead (microseconds)', fontsize=12, fontweight='bold')
plt.title('Benchmark: Scheduling Overhead vs Blocking Tasks', fontsize=14, fontweight='bold')
plt.legend(fontsize=11)
plt.grid(True, alpha=0.3)
plt.yscale('log')  # Log scale because p99 jumps from 1.5ms to 140ms

# Add annotation about the cliff
plt.annotate('p50 is blind to the problem.\np99 shows the cliff.',
             xy=(3, 2000), xytext=(3.5, 10000),
             arrowprops=dict(facecolor='black', shrink=0.05, width=1.5, headwidth=8),
             fontsize=11, bbox=dict(boxstyle='round,pad=0.5', facecolor='yellow', alpha=0.3))

plt.tight_layout()
plt.savefig('docs/benchmark_scheduling_cliff.png', dpi=300, bbox_inches='tight')
print("Graph 2 saved as docs/benchmark_scheduling_cliff.png")
