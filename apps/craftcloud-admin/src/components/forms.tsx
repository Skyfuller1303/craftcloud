import { useEffect, useRef } from "react";
import type { ReactNode, RefObject } from "react";
import { FieldError, Input, Label, Text, TextField } from "react-aria-components";

export function Field({
  label,
  name,
  value,
  onChange,
  type = "text",
  error,
  description,
  autoComplete,
  required = true,
}: {
  label: string;
  name: string;
  value: string;
  onChange: (value: string) => void;
  type?: "text" | "email" | "password";
  error?: string | undefined;
  description?: string;
  autoComplete?: string;
  required?: boolean;
}) {
  return (
    <TextField
      className="field"
      name={name}
      type={type}
      value={value}
      onChange={onChange}
      isRequired={required}
      isInvalid={error !== undefined}
      validationBehavior="aria"
      {...(autoComplete === undefined ? {} : { autoComplete })}
    >
      <Label className="field__label">{label}</Label>
      <Input className="field__input" spellCheck={false} autoCapitalize="none" />
      {description !== undefined && (
        <Text slot="description" className="field__hint">
          {description}
        </Text>
      )}
      <FieldError className="field__error">{error}</FieldError>
    </TextField>
  );
}

/** Message block announced to assistive technology when it appears. */
export function Alert({
  tone = "error",
  children,
}: {
  tone?: "error" | "info" | "success";
  children: ReactNode;
}) {
  return (
    <div className={`alert alert--${tone}`} role={tone === "error" ? "alert" : "status"}>
      {children}
    </div>
  );
}

/** Moves keyboard focus to the screen heading when a screen appears. */
export function useFocusOnMount<T extends HTMLElement>(): RefObject<T | null> {
  const ref = useRef<T | null>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  return ref;
}

export function ScreenHeading({ children }: { children: ReactNode }) {
  const ref = useFocusOnMount<HTMLHeadingElement>();
  return (
    <h1 className="screen__title" tabIndex={-1} ref={ref}>
      {children}
    </h1>
  );
}

/** After a failed submit, focuses the first invalid field of the form. */
export function focusFirstInvalid(form: HTMLFormElement | null): void {
  form?.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus();
}
