import React from "react";

interface TranscriptionIconProps {
  width?: number;
  height?: number;
  color?: string;
  className?: string;
}

const TranscriptionIcon: React.FC<TranscriptionIconProps> = ({
  width = 24,
  height = 24,
  color = "#90E4C1",
  className = "",
}) => {
  return (
    <svg
      width={width}
      height={height}
      viewBox="0 0 24 24"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className={className}
    >
      <path
        d="M3.25 12.25h3.2l1.55-4.5 3.15 9.5 2.95-9.5 1.65 4.5h5"
        stroke={color}
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <rect x="9.35" y="3.75" width="5.3" height="8.5" rx="2.65" fill={color} />
      <path
        d="M7.5 9.8c0 2.55 1.98 4.62 4.5 4.62s4.5-2.07 4.5-4.62"
        stroke={color}
        strokeWidth="1.65"
        strokeLinecap="round"
      />
      <path
        d="M12 14.42v2.33"
        stroke={color}
        strokeWidth="1.65"
        strokeLinecap="round"
      />
    </svg>
  );
};

export default TranscriptionIcon;
