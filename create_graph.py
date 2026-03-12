import matplotlib.pyplot as plt
import numpy as np

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
plt.savefig('blocking_failure_cliff.png', dpi=300, bbox_inches='tight')
print("Graph saved as blocking_failure_cliff.png")
