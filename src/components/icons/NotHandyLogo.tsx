import { APP_NAME } from "@/lib/brand";
import logoSrc from "@/assets/not-handy-logo.png";

interface NotHandyLogoProps {
  width?: number | string;
  className?: string;
}

const NotHandyLogo = ({ width, className = "" }: NotHandyLogoProps) => {
  const numericWidth = typeof width === "number" ? width : undefined;
  const compact = numericWidth !== undefined && numericWidth < 150;
  const iconSize = compact ? 34 : 52;

  return (
    <div
      className={`flex items-center justify-center gap-2 ${className}`}
      style={width ? { width } : undefined}
      aria-label={APP_NAME}
    >
      <img
        src={logoSrc}
        alt=""
        width={iconSize}
        height={iconSize}
        className="shrink-0 rounded-lg"
      />
      <span
        className="font-semibold text-text whitespace-nowrap"
        style={{
          fontSize: compact ? 13 : 22,
          lineHeight: compact ? "18px" : "28px",
        }}
      >
        {APP_NAME}
      </span>
    </div>
  );
};

export default NotHandyLogo;
