import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";

export interface DropdownOption {
  value: string;
  label: string;
  description?: string;
  disabled?: boolean;
}

interface DropdownProps {
  options: DropdownOption[];
  className?: string;
  menuClassName?: string;
  selectedValue: string | null;
  onSelect: (value: string) => void;
  placeholder?: string;
  disabled?: boolean;
  onRefresh?: () => void;
}

const MENU_MAX_HEIGHT = 240;
const GAP = 4;

export const Dropdown: React.FC<DropdownProps> = ({
  options,
  selectedValue,
  onSelect,
  className = "",
  menuClassName,
  placeholder = "Select an option...",
  disabled = false,
  onRefresh,
}) => {
  const { t } = useTranslation();
  const [isOpen, setIsOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [menuStyle, setMenuStyle] = useState<React.CSSProperties>({});

  // Positioned against the viewport rather than the trigger's offset parent.
  // Absolute placement kept the menu inside the settings scroll container, which
  // clipped it whenever the trigger sat near the bottom of a page.
  const position = useCallback(() => {
    const trigger = dropdownRef.current;
    if (!trigger) return;

    const rect = trigger.getBoundingClientRect();
    const below = window.innerHeight - rect.bottom - GAP;
    const above = rect.top - GAP;
    const dropUp = below < Math.min(MENU_MAX_HEIGHT, above);
    const available = Math.max(96, dropUp ? above : below);

    // `minWidth`, not `width`: a caller can widen the menu past its trigger
    // with `menuClassName`, and an inline width would override that class.
    const menuWidth = menuRef.current?.offsetWidth ?? rect.width;
    // Keeps a menu wider than its trigger on screen. This is what upstream's
    // `right-0` bought before the menu became position:fixed.
    const left = Math.max(
      GAP,
      Math.min(rect.left, window.innerWidth - menuWidth - GAP),
    );

    setMenuStyle({
      position: "fixed",
      left,
      minWidth: rect.width,
      maxHeight: Math.min(MENU_MAX_HEIGHT, available),
      ...(dropUp
        ? { bottom: window.innerHeight - rect.top + GAP }
        : { top: rect.bottom + GAP }),
    });
  }, []);

  useLayoutEffect(() => {
    if (!isOpen) return;
    position();

    // `true` so ancestor scrolls are caught, not just the window's.
    window.addEventListener("scroll", position, true);
    window.addEventListener("resize", position);
    return () => {
      window.removeEventListener("scroll", position, true);
      window.removeEventListener("resize", position);
    };
  }, [isOpen, position]);

  useEffect(() => {
    if (!isOpen) return;

    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      // The menu is no longer a descendant of the trigger, so both count as
      // inside.
      if (
        dropdownRef.current?.contains(target) ||
        menuRef.current?.contains(target)
      ) {
        return;
      }
      setIsOpen(false);
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setIsOpen(false);
    };

    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [isOpen]);

  const selectedOption = options.find(
    (option) => option.value === selectedValue,
  );

  const handleSelect = (value: string) => {
    onSelect(value);
    setIsOpen(false);
  };

  const handleToggle = () => {
    if (disabled) return;
    if (!isOpen && onRefresh) onRefresh();
    setIsOpen(!isOpen);
  };

  return (
    <div className={`relative ${className}`} ref={dropdownRef}>
      <button
        type="button"
        className={`px-2 py-[5px] text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 rounded-md min-w-[200px] w-full text-start grid grid-cols-[1fr_auto] gap-2 items-center transition-all duration-150 ${
          disabled
            ? "opacity-50 cursor-not-allowed"
            : "hover:bg-logo-primary/10 cursor-pointer hover:border-logo-primary"
        }`}
        onClick={handleToggle}
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={isOpen}
      >
        <span className="truncate">{selectedOption?.label || placeholder}</span>
        <svg
          className={`w-4 h-4 transition-transform duration-200 ${isOpen ? "transform rotate-180" : ""}`}
          fill="none"
          stroke="currentColor"
          viewBox="0 0 24 24"
        >
          <path
            strokeLinecap="round"
            strokeLinejoin="round"
            strokeWidth={2}
            d="M19 9l-7 7-7-7"
          />
        </svg>
      </button>
      {isOpen &&
        !disabled &&
        createPortal(
          <div
            ref={menuRef}
            role="listbox"
            style={menuStyle}
            // Above the dialog's z-50, so a dropdown opened inside a modal
            // overlays it instead of hiding behind the backdrop.
            className={`bg-background border border-mid-gray/80 rounded-md shadow-lg z-60 overflow-y-auto ${
              menuClassName ?? ""
            }`}
          >
            {options.length === 0 ? (
              <div className="px-2 py-1 text-sm text-mid-gray">
                {t("common.noOptionsFound")}
              </div>
            ) : (
              options.map((option) => (
                <button
                  key={option.value}
                  type="button"
                  role="option"
                  aria-selected={selectedValue === option.value}
                  className={`w-full text-sm text-start hover:bg-logo-primary/10 transition-colors duration-150 ${
                    option.description ? "px-3 py-2" : "px-2 py-1"
                  } ${
                    selectedValue === option.value ? "bg-logo-primary/20" : ""
                  } ${option.disabled ? "opacity-50 cursor-not-allowed" : ""}`}
                  onClick={() => handleSelect(option.value)}
                  disabled={option.disabled}
                >
                  <span
                    className={`block whitespace-normal break-words ${
                      option.description || selectedValue === option.value
                        ? "font-semibold"
                        : ""
                    }`}
                  >
                    {option.label}
                  </span>
                  {option.description && (
                    <span className="mt-0.5 block whitespace-normal text-xs font-normal leading-snug text-mid-gray">
                      {option.description}
                    </span>
                  )}
                </button>
              ))
            )}
          </div>,
          document.body,
        )}
    </div>
  );
};
