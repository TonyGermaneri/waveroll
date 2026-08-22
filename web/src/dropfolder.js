/**
 * Writing staged takes into a folder the DAW is already watching.
 *
 * A web page cannot hand another application a file *path* — only a promise, which Ableton
 * declines — and no API changes that. But it can write a real file into a real directory, and
 * Live indexes anything under its User Library or any folder added to Places. Point this at one
 * of those and a staged take appears in Live's own browser a moment later, where dragging into
 * the arrangement is native and works exactly as it should.
 *
 * So the drag does not come from the web page at all. The page just puts the file where the DAW
 * is already looking.
 *
 * Chromium only (File System Access API), and the handle is kept in IndexedDB so the folder is
 * chosen once rather than once per session. Chrome will still ask to confirm access on a new
 * visit unless the app is installed, which is one of the better reasons here to install it.
 */

const DB = 'waveroll';
const STORE = 'handles';
const KEY = 'dropFolder';

function idb() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function store(handle) {
  const db = await idb();
  await new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).put(handle, KEY);
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
  });
}

async function load() {
  const db = await idb();
  return new Promise((resolve) => {
    const tx = db.transaction(STORE, 'readonly');
    const request = tx.objectStore(STORE).get(KEY);
    request.onsuccess = () => resolve(request.result ?? null);
    request.onerror = () => resolve(null);
  });
}

export const supported =
  typeof window !== 'undefined' && typeof window.showDirectoryPicker === 'function';

export class DropFolder {
  constructor() {
    this.handle = null;
  }

  get name() {
    return this.handle?.name ?? null;
  }

  /**
   * Restores a previously chosen folder, if permission is still granted.
   *
   * Deliberately does *not* request permission: that needs a user gesture, and prompting on load
   * for something the user may not be about to do is how permission prompts get dismissed
   * reflexively. Returns false when a folder is remembered but needs a click to re-authorise.
   */
  async restore() {
    const handle = await load();
    if (!handle) return false;
    const state = await handle.queryPermission({ mode: 'readwrite' });
    if (state === 'granted') {
      this.handle = handle;
      return true;
    }
    this.pending = handle;
    return false;
  }

  /** Re-authorises a remembered folder, or picks a new one. Must be called from a gesture. */
  async choose(reuse = true) {
    if (reuse && this.pending) {
      const state = await this.pending.requestPermission({ mode: 'readwrite' });
      if (state === 'granted') {
        this.handle = this.pending;
        this.pending = null;
        return this.handle.name;
      }
    }
    const handle = await window.showDirectoryPicker({
      id: 'waveroll-drop',
      mode: 'readwrite',
      // Live's User Library lives under Music by default, so this opens near it.
      startIn: 'music',
    });
    this.handle = handle;
    await store(handle);
    return handle.name;
  }

  forget() {
    this.handle = null;
    this.pending = null;
  }

  /**
   * Writes `bytes` as `name`, never overwriting: a second take called the same thing gets a
   * suffix rather than replacing the first, because the first may already be in somebody's set
   * and replacing a file a DAW is referencing is how a project quietly changes underneath them.
   */
  async write(name, bytes) {
    if (!this.handle) throw new Error('no drop folder');
    const unique = await this.uniqueName(name);
    const file = await this.handle.getFileHandle(unique, { create: true });
    const stream = await file.createWritable();
    await stream.write(bytes);
    await stream.close();
    return unique;
  }

  async uniqueName(name) {
    const dot = name.lastIndexOf('.');
    const stem = dot > 0 ? name.slice(0, dot) : name;
    const extension = dot > 0 ? name.slice(dot) : '';
    for (let n = 0; n < 1000; n++) {
      const candidate = n === 0 ? name : `${stem}_${String(n + 1).padStart(2, '0')}${extension}`;
      try {
        await this.handle.getFileHandle(candidate);
      } catch {
        return candidate; // getFileHandle throws when it does not exist, which is the answer
      }
    }
    return `${stem}_${Date.now()}${extension}`;
  }
}
