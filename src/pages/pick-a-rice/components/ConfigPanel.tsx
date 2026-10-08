import { useEffect, useRef, useState } from 'react';
import styles from './ConfigPanel.module.css';
import type { InstallConfig } from '@/shared/backend';

interface Props {
  config: InstallConfig;
  onDismiss: () => void;
}

/**
 * The configuration an `install` produced.
 *
 * On a declarative platform `install` cannot mutate the system, so what it
 * produces is code the user adopts. That makes the config the *result* of the
 * install rather than a detail, which is why this is a panel the user dismisses
 * instead of an overlay that times out — the text has to survive being read.
 */
export function ConfigPanel({ config, onDismiss }: Props) {
  const [copied, setCopied] = useState(false);
  const copyTimeoutRef = useRef<ReturnType<typeof window.setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (copyTimeoutRef.current !== null) window.clearTimeout(copyTimeoutRef.current);
    },
    [],
  );

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(config.text);
      setCopied(true);
      if (copyTimeoutRef.current !== null) window.clearTimeout(copyTimeoutRef.current);
      copyTimeoutRef.current = window.setTimeout(() => {
        copyTimeoutRef.current = null;
        setCopied(false);
      }, 1500);
    } catch (error) {
      // The panel is the fallback: the text is on screen and selectable.
      console.warn('[rice-cooker] clipboard write failed:', error);
    }
  };

  return (
    <div className={styles.wrap} role="dialog" aria-label="Configuration to adopt">
      <div className={styles.panel}>
        <div className={styles.header}>
          <span className={styles.title}>add this to your configuration</span>
          <button type="button" className={styles.close} onClick={onDismiss}>
            esc
          </button>
        </div>

        <p className={styles.note}>
          Rice Cooker cannot change a declarative system, so this is the install:
          paste it, then rebuild.
        </p>

        <pre className={styles.code} onClick={copy} title="click to copy">
          {config.text}
        </pre>

        <div className={styles.footer}>
          <span className={styles.path}>{config.path ?? ''}</span>
          <span className={styles.copied} data-copied={copied}>
            {copied ? 'copied' : 'click the code to copy'}
          </span>
        </div>
      </div>
    </div>
  );
}
