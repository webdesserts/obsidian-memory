/**
 * VaultOperationQueue - Serializes all WASM vault operations.
 *
 * The WASM vault uses `&mut self` for async operations, which means JavaScript
 * cannot safely call multiple methods concurrently. This queue ensures all
 * operations run sequentially, preventing:
 * - Rust borrow checker panics from aliased mutable references
 * - Data races on the internal document cache
 * - TOCTOU bugs in document loading
 */
export class VaultOperationQueue {
  private queue: Promise<void> = Promise.resolve();
  private pending = 0;

  /**
   * Run an operation on the vault sequentially.
   *
   * Operations are queued and executed one at a time. If an operation fails,
   * the error is propagated but the queue continues processing.
   */
  async run<T>(operation: () => Promise<T>): Promise<T> {
    this.pending++;

    return new Promise<T>((resolve, reject) => {
      this.queue = this.queue
        .then(async () => {
          try {
            const result = await operation();
            resolve(result);
          } catch (err) {
            reject(err);
          } finally {
            this.pending--;
          }
        })
        .catch(() => {
          // Errors are handled per-operation, don't break the queue
          this.pending--;
        });
    });
  }

  /**
   * Check if there are pending operations.
   */
  get hasPending(): boolean {
    return this.pending > 0;
  }

  /**
   * Get the number of pending operations.
   */
  get pendingCount(): number {
    return this.pending;
  }
}
