import { motion } from 'framer-motion';
import BackIcon from '@/assets/icons/back.svg?react';
import DownloadIcon from '@/assets/preview-actions/download.svg?react';
import GithubIcon from '@/assets/icons/github.svg?react';
import PointerIcon from '@/assets/icons/pointer.svg?react';
import styles from './PreviewContent.module.css';
import { SHRUNKEN_TEXT_VARIANTS, usePreviewOption, useView, type PreviewOption } from '../view';

interface PreviewContentProps {
  themeName: string;
  creatorName: string;
  installSupported: boolean;
  /** Rices in the catalog that this compositor cannot run. */
  hiddenCount: number;
  hiddenReason?: string;
  onApply: () => void;
}

const WORDS: Record<PreviewOption, string> = {
  leave: 'LEAVE',
  install: 'INSTALL',
  dots: 'DOTS',
};
const ICONS = {
  leave: BackIcon,
  install: DownloadIcon,
  dots: GithubIcon,
} as const;

export function PreviewContent({
  themeName,
  creatorName,
  installSupported,
  hiddenCount,
  hiddenReason,
  onApply,
}: PreviewContentProps) {
  const view = useView();
  const option = usePreviewOption();
  const active = view === 'preview';
  const installUnavailable = option === 'install' && !installSupported;

  return (
    <motion.div
      className={styles.wrap}
      initial={false}
      animate={active ? 'visible' : 'hidden'}
      variants={SHRUNKEN_TEXT_VARIANTS}
      style={{ pointerEvents: active ? 'auto' : 'none' }}
    >
      {hiddenCount > 0 ? (
        // Loud rather than silent: a rice that cannot run here is hidden, and
        // saying so beats an unexplained shorter list.
        <p className={styles.hiddenNote}>
          {hiddenCount} {hiddenCount === 1 ? 'rice' : 'rices'} hidden
          {hiddenReason ? `: ${hiddenReason}` : ''}
        </p>
      ) : null}
      <p className={`${styles.navLabel} ${styles.prevLabel}`}>Prev</p>
      <p className={`${styles.navLabel} ${styles.nextLabel}`}>Next</p>
      <p className={`${styles.navLabel} ${styles.confirmLabel}`}>CONFIRM</p>

      <ActionButton type="leave" active={option === 'leave'} />
      <ActionButton
        type="install"
        active={option === 'install'}
        unavailable={!installSupported}
      />
      <ActionButton type="dots" active={option === 'dots'} />
      <OptionPointers option={option} />

      <button
        type="button"
        tabIndex={-1}
        className={`${styles.wordPill} ${styles[`word_${option}`]} ${
          installUnavailable ? styles.wordUnavailable : ''
        }`}
        onMouseDown={(event) => event.preventDefault()}
        onClick={(event) => {
          event.stopPropagation();
          onApply();
        }}
      >
        {installUnavailable ? (
          <span className={styles.unavailableText}>Installation not available</span>
        ) : (
          <span className={styles.wordCluster}>
            {WORDS[option].split('').map((char, index) => (
              <span key={index} className={styles.wordLetter}>
                {char}
              </span>
            ))}
          </span>
        )}
      </button>

      <p className={styles.metaLabel}>
        {themeName} by {creatorName}
      </p>
    </motion.div>
  );
}

function ActionButton({
  type,
  active,
  unavailable = false,
}: {
  type: PreviewOption;
  active: boolean;
  unavailable?: boolean;
}) {
  const className = `${styles.actionBtn} ${styles[`action_${type}`]} ${
    active ? styles.actionActive : ''
  } ${unavailable ? styles.actionUnavailable : ''}`;
  const Icon = ICONS[type];
  return (
    <span className={className} aria-hidden="true">
      <Icon className={styles.actionIcon} />
    </span>
  );
}

function OptionPointers({ option }: { option: PreviewOption }) {
  if (option === 'install') {
    return (
      <>
        <PointerIcon
          aria-hidden="true"
          className={`${styles.pointer} ${styles.pointerInstallTop}`}
        />
        <PointerIcon
          aria-hidden="true"
          className={`${styles.pointer} ${styles.pointerInstallBottom}`}
        />
      </>
    );
  }
  return (
    <PointerIcon
      aria-hidden="true"
      className={`${styles.pointer} ${
        option === 'leave' ? styles.pointerLeave : styles.pointerDots
      }`}
    />
  );
}
