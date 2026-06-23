export function SkeletonRows({
  count = 3,
  className = "skeleton-rows",
}: {
  count?: number;
  className?: string;
}) {
  return (
    <div aria-hidden="true" className={className}>
      {Array.from({ length: count }, (_, index) => (
        <span
          className="skeleton-row"
          key={index}
          style={{ width: `${100 - index * 12}%` }}
        />
      ))}
    </div>
  );
}
