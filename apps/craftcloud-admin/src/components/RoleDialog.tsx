import { useState } from "react";
import { Button } from "react-aria-components";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { NativeSelect } from "./ContextSelect.tsx";
import { AppModal } from "./Dialog.tsx";
import { ErrorNotice } from "./ErrorNotice.tsx";

/**
 * Pick one of the roles the viewer may assign. The server decides again; a 403
 * after a role change elsewhere is shown inside the dialog with a reload action.
 */
export function RoleDialog<R extends string>({
  title,
  who,
  current,
  choices,
  roleName,
  onSave,
  onClose,
  onReload,
}: {
  title: string;
  who: string;
  current: R;
  choices: R[];
  roleName: (role: R) => string;
  onSave: (role: R) => Promise<void>;
  onClose: () => void;
  onReload: () => void;
}) {
  const { t } = useI18n();
  const [role, setRole] = useState<R>(current);
  const action = useAction();

  async function save() {
    const done = await action.run(async () => {
      await onSave(role);
      return true;
    });
    if (done === true) {
      onClose();
    }
  }

  return (
    <AppModal title={title} onClose={onClose} busy={action.pending}>
      <p>{who}</p>
      <NativeSelect
        label={t("Role")}
        value={role}
        options={choices.map((choice) => ({ id: choice, label: roleName(choice) }))}
        onChange={(value) => {
          const next = choices.find((choice) => choice === value);
          if (next !== undefined) {
            setRole(next);
          }
        }}
      />
      {action.failed && <ErrorNotice error={action.error} onRetry={() => void save()} onReload={onReload} />}
      <div className="form__actions">
        <Button className="button button--primary" onPress={() => void save()} isDisabled={action.pending}>
          {t("Save")}
        </Button>
        <Button className="button" onPress={onClose} isDisabled={action.pending}>
          {t("Cancel")}
        </Button>
      </div>
    </AppModal>
  );
}
