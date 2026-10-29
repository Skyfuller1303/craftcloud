import {
  Button,
  Label,
  ListBox,
  ListBoxItem,
  Popover,
  Select,
  SelectValue,
} from "react-aria-components";

export interface Choice {
  id: string;
  label: string;
}

/** Accessible dropdown (React Aria Select) used for the organization and workspace pickers. */
export function ContextSelect({
  label,
  choices,
  selectedId,
  onSelect,
  placeholder,
  isDisabled = false,
}: {
  label: string;
  choices: Choice[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  placeholder: string;
  isDisabled?: boolean;
}) {
  return (
    <Select
      className="select"
      selectedKey={selectedId}
      onSelectionChange={(key) => {
        if (key !== null) {
          onSelect(String(key));
        }
      }}
      placeholder={placeholder}
      isDisabled={isDisabled}
    >
      <Label className="select__label">{label}</Label>
      <Button className="select__button">
        <SelectValue className="select__value" />
        <span aria-hidden="true" className="select__chevron">
          ▾
        </span>
      </Button>
      <Popover className="select__popover">
        <ListBox className="select__list">
          {choices.map((choice) => (
            <ListBoxItem key={choice.id} id={choice.id} textValue={choice.label} className="select__item">
              {choice.label}
            </ListBoxItem>
          ))}
        </ListBox>
      </Popover>
    </Select>
  );
}

/** Plain native selector, used for the language and theme switches. */
export function NativeSelect({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string;
  options: Choice[];
  onChange: (value: string) => void;
}) {
  return (
    <label className="select">
      <span className="select__label">{label}</span>
      <select
        className="select__native"
        value={value}
        onChange={(event) => onChange(event.target.value)}
      >
        {options.map((option) => (
          <option key={option.id} value={option.id}>
            {option.label}
          </option>
        ))}
      </select>
    </label>
  );
}
